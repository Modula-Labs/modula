pub mod meta;
pub mod middleware;
pub mod router;
pub mod spa;

use std::time::Duration;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tower_http::trace::TraceLayer;

use crate::state::AppState;

pub use router::{Access, ApiRouter, RouteSpec};

const MAX_BODY: usize = 1024 * 1024;

pub fn app(state: AppState) -> Router {
    let (api, _) = crate::api::routes(state.clone()).finish();
    api.fallback(spa::serve)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::require_same_origin,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::security_headers,
        ))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(axum::middleware::from_fn(timeout))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Every request must produce its response within 30 seconds, except source
/// uploads: those read a large body and enforce their own idle timeout.
async fn timeout(req: Request, next: Next) -> Response {
    let upload = req.method() == Method::POST && req.uri().path().ends_with("/uploads");
    if upload {
        return next.run(req).await;
    }
    match tokio::time::timeout(Duration::from_secs(30), next.run(req)).await {
        Ok(res) => res,
        Err(_) => StatusCode::REQUEST_TIMEOUT.into_response(),
    }
}

/// The full route table, for policy tests.
pub fn route_specs(state: AppState) -> Vec<RouteSpec> {
    crate::api::routes(state).finish().1
}
