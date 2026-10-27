use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::auth::setup;
use crate::error::AppResult;
use crate::state::AppState;

pub async fn health() -> &'static str {
    "ok"
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceStatus {
    pub setup_required: bool,
    pub version: &'static str,
}

pub async fn status(State(state): State<AppState>) -> AppResult<Json<InstanceStatus>> {
    Ok(Json(InstanceStatus {
        setup_required: setup::is_pending(&state.db).await?,
        version: env!("CARGO_PKG_VERSION"),
    }))
}
