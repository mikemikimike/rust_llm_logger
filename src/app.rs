//! Router construction shared by `main` and the end-to-end tests.

use axum::{
    extract::{Query, Request, State},
    http::header,
    middleware::{from_fn, from_fn_with_state, Next},
    response::{Html, IntoResponse, Response},
    routing::{any, get},
    Extension, Router,
};
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::Arc;
use tower_http::trace::TraceLayer;

use crate::auth::{self, AccessConfig};
use crate::telemetry::Telemetry;
use crate::{dashboard, middleware, proxy};

pub type HttpClient = hyper_util::client::legacy::Client<
    hyper_util::client::legacy::connect::HttpConnector,
    axum::body::Body,
>;

#[derive(Clone)]
pub struct AppState {
    pub client: Arc<HttpClient>,
    pub telemetry: Arc<Telemetry>,
}

pub fn create_http_client() -> HttpClient {
    hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build_http()
}

pub fn build_app(allow: Arc<HashSet<u16>>, telemetry: Arc<Telemetry>) -> Router {
    build_app_with_access(allow, telemetry, AccessConfig::default())
}

pub fn build_app_with_access(
    allow: Arc<HashSet<u16>>,
    telemetry: Arc<Telemetry>,
    access: AccessConfig,
) -> Router {
    let access = Arc::new(access);
    let state = AppState {
        client: Arc::new(create_http_client()),
        telemetry,
    };

    let proxy_routes = Router::new()
        .route("/proxy/:backend_port/*path", any(proxy::proxy_handler))
        .layer(from_fn(middleware::extract_request_data))
        .layer(from_fn_with_state(allow, allow_port_middleware));

    Router::new()
        .route("/", get(dashboard_handler))
        .route("/metrics", get(metrics_handler))
        .merge(proxy_routes)
        .layer(from_fn_with_state(Arc::clone(&access), auth::authorize))
        .layer(Extension(access))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[derive(Deserialize)]
struct DashQuery {
    model: Option<String>,
    limit: Option<usize>,
}

async fn dashboard_handler(
    State(s): State<AppState>,
    Extension(access): Extension<Arc<AccessConfig>>,
    Query(q): Query<DashQuery>,
) -> Html<String> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let model = q.model.filter(|m| !m.is_empty());
    let telemetry = Arc::clone(&s.telemetry);
    let m2 = model.clone();
    let rows = tokio::task::spawn_blocking(move || telemetry.recent(limit, m2.as_deref()))
        .await
        .unwrap_or_default();
    Html(dashboard::render_with_prompts(
        &rows,
        model.as_deref(),
        access.show_prompts,
    ))
}

async fn metrics_handler(State(s): State<AppState>) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        s.telemetry.prom.render(),
    )
        .into_response()
}

async fn allow_port_middleware(
    State(allow): State<Arc<HashSet<u16>>>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    if let Some(rest) = path.strip_prefix("/proxy/") {
        let port_str = rest.split('/').next().unwrap_or("");
        if let Ok(port) = port_str.parse::<u16>() {
            if !allow.contains(&port) {
                return (
                    axum::http::StatusCode::FORBIDDEN,
                    format!("backend port {port} not in ALLOWED_BACKEND_PORTS"),
                )
                    .into_response();
            }
        } else if !port_str.is_empty() {
            return (axum::http::StatusCode::BAD_REQUEST, "invalid backend port").into_response();
        }
    }
    next.run(req).await
}
