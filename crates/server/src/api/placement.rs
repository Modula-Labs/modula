//! Where a new service would run: the same ranking the first deployment
//! uses (deploy/placement.rs), so the dashboard's server picker agrees.

use axum::Json;
use axum::extract::{Query, State};
use modula_id::Id;
use serde::{Deserialize, Serialize};

use crate::authz::Grant;
use crate::deploy::placement;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SuggestionQuery {
    /// The service's instance when it exists (its variables' references
    /// count, and it isn't its own neighbour).
    pub instance: Option<Id>,
    /// Service names it will reference, comma-separated.
    #[serde(default)]
    pub references: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SuggestedServer {
    pub node_id: Id,
    pub name: String,
    pub online: bool,
    /// Runs the control plane.
    pub main: bool,
    pub location_id: Option<String>,
    /// The first one: where the service goes unless someone picks another.
    pub recommended: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlacementSuggestion {
    /// Best first.
    pub servers: Vec<SuggestedServer>,
}

pub async fn suggestion(
    State(state): State<AppState>,
    grant: Grant,
    Query(q): Query<SuggestionQuery>,
) -> AppResult<Json<PlacementSuggestion>> {
    let environment = grant.id();
    if let Some(i) = q.instance {
        let here: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM service_instances WHERE id = $1 AND environment_id = $2)",
        )
        .bind(i)
        .bind(environment)
        .fetch_one(&state.db)
        .await?;
        if !here {
            return Err(AppError::bad_request("no such service in this environment"));
        }
    }
    let references: Vec<String> = q
        .references
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    let mut conn = state.db.acquire().await?;
    let ranked = placement::suggest(&mut conn, environment, q.instance, &references).await?;
    Ok(Json(PlacementSuggestion {
        servers: ranked
            .into_iter()
            .enumerate()
            .map(|(i, s)| SuggestedServer {
                node_id: s.id,
                name: s.name,
                online: s.online,
                main: s.main,
                location_id: s.location,
                recommended: i == 0,
            })
            .collect(),
    }))
}
