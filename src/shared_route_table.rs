use std::sync::Arc;

use anyhow::{Context, Error, Result};
use graphgate_core::{
    ComposedSchema, Executor, PlanBuilder, Request, Response, ServerError, ServiceRouteTable,
};
use serde::Deserialize;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{Duration, Instant};
use value::ConstValue;
use warp::http::{HeaderMap, Response as HttpResponse, StatusCode};

enum Command {
    Change(ServiceRouteTable),
}

struct Inner {
    schema: Option<Arc<ComposedSchema>>,
    route_table: Option<Arc<ServiceRouteTable>>,
}

#[derive(Clone)]
pub struct SharedRouteTable {
    inner: Arc<Mutex<Inner>>,
    tx: mpsc::UnboundedSender<Command>,
}

impl Default for SharedRouteTable {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let shared_route_table = Self {
            inner: Arc::new(Mutex::new(Inner {
                schema: None,
                route_table: None,
            })),
            tx,
        };
        tokio::spawn({
            let shared_route_table = shared_route_table.clone();
            async move { shared_route_table.update_loop(rx).await }
        });
        shared_route_table
    }
}

impl SharedRouteTable {
    async fn update_loop(self, mut rx: mpsc::UnboundedReceiver<Command>) {
        let mut update_interval = tokio::time::interval_at(Instant::now(), Duration::from_secs(1));

        loop {
            tokio::select! {
                _ = update_interval.tick() => {
                    if let Err(err) = self.update().await {
                        tracing::error!(error = %err, "Failed to update schema.");
                    }
                }
                command = rx.recv() => {
                    if let Some(command) = command {
                        match command {
                            Command::Change(route_table) => {
                                let mut inner = self.inner.lock().await;
                                inner.route_table = Some(Arc::new(route_table));
                                inner.schema = None;
                            }
                        }
                    }
                }
            }
        }
    }

    async fn update(&self) -> Result<()> {
        const QUERY_SDL: &str = "{ _service { sdl }}";

        #[derive(Deserialize)]
        struct ResponseQuery {
            #[serde(rename = "_service")]
            service: ResponseService,
        }

        #[derive(Deserialize)]
        struct ResponseService {
            sdl: String,
        }

        let route_table = match self.inner.lock().await.route_table.clone() {
            Some(route_table) => route_table,
            None => return Ok(()),
        };

        let resp = futures_util::future::try_join_all(route_table.keys().map(|service| {
            let route_table = route_table.clone();
            async move {
                let resp =
                    graphgate_core::query(&route_table, service, Request::new(QUERY_SDL), None)
                        .await
                        .with_context(|| format!("Failed to fetch SDL from '{}'.", service))?;
                let resp: ResponseQuery =
                    value::from_value(resp.data).context("Failed to parse response.")?;
                let document = parser::parse_schema(resp.service.sdl)
                    .with_context(|| format!("Invalid SDL from '{}'.", service))?;
                Ok::<_, Error>((service.to_string(), document))
            }
        }))
        .await?;

        let schema = ComposedSchema::combine(resp)?;
        self.inner.lock().await.schema = Some(Arc::new(schema));
        Ok(())
    }

    pub fn set_route_table(&self, route_table: ServiceRouteTable) {
        self.tx.send(Command::Change(route_table)).ok();
    }

    pub async fn get_inner(&self) -> Option<(Arc<ComposedSchema>, Arc<ServiceRouteTable>)> {
        let (composed_schema, route_table) = {
            let inner = self.inner.lock().await;
            (inner.schema.clone(), inner.route_table.clone())
        };
        composed_schema.zip(route_table)
    }

    pub async fn query(&self, request: Request, header_map: HeaderMap) -> HttpResponse<String> {
        let document = match parser::parse_query(request.query) {
            Ok(document) => document,
            Err(err) => {
                return HttpResponse::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(err.to_string())
                    .unwrap();
            }
        };

        let (composed_schema, route_table) = match self.get_inner().await {
            Some((composed_schema, route_table)) => (composed_schema, route_table),
            _ => {
                return HttpResponse::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(
                        serde_json::to_string(&Response {
                            data: ConstValue::Null,
                            errors: vec![ServerError::new("Not ready.")],
                        })
                        .unwrap(),
                    )
                    .unwrap();
            }
        };

        let mut plan_builder =
            PlanBuilder::new(&composed_schema, document).variables(request.variables);
        if let Some(operation) = request.operation {
            plan_builder = plan_builder.operation_name(operation);
        }

        let plan = match plan_builder.plan() {
            Ok(plan) => plan,
            Err(response) => {
                return HttpResponse::builder()
                    .status(StatusCode::OK)
                    .body(serde_json::to_string(&response).unwrap())
                    .unwrap();
            }
        };

        let executor = Executor::new(&composed_schema, &*route_table).with_headers(&header_map);
        HttpResponse::builder()
            .status(StatusCode::OK)
            .body(serde_json::to_string(&executor.execute(&plan).await).unwrap())
            .unwrap()
    }
}