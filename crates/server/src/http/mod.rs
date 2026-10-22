pub mod meta;
pub mod middleware;
pub mod router;
pub mod spa;

use std::time::Duration;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use tower_http::timeout::TimeoutLayer;
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
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(30),
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// The full route table, for policy tests.
pub fn route_specs(state: AppState) -> Vec<RouteSpec> {
    crate::api::routes(state).finish().1
}
