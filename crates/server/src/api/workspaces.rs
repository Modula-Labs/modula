use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use super::auth::{Membership, create_workspace_tx};
use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;
use crate::validate;

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub id: Uuid,
    pub name: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

pub async fn list(State(state): State<AppState>, actor: Actor) -> AppResult<Json<Vec<Membership>>> {
    Ok(Json(
        super::auth::load_me(&state.db, actor.user_id)
            .await?
            .workspaces,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateWorkspace {
    pub name: String,
}

pub async fn create(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
    Json(req): Json<CreateWorkspace>,
) -> AppResult<Json<Workspace>> {
    let name = validate::display_name(&req.name, "workspace name")?;
    let id = Uuid::now_v7();
    let mut tx = state.db.begin().await?;
    create_workspace_tx(&mut tx, id, &name, actor.user_id).await?;
    Entry::new("workspace.create", &meta)
        .actor(&actor)
        .workspace(id)
        .target("workspace", id)
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    fetch(&state, id).await.map(Json)
}

async fn fetch(state: &AppState, id: Uuid) -> AppResult<Workspace> {
    sqlx::query_as("SELECT id, name, created_at FROM workspaces WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)
}

pub async fn get(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Workspace>> {
    let ws = grant.workspace_id()?;
    fetch(&state, ws).await.map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateWorkspace {
    pub name: String,
}

pub async fn update(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<UpdateWorkspace>,
) -> AppResult<Json<Workspace>> {
    let ws = grant.workspace_id()?;
    let name = validate::display_name(&req.name, "workspace name")?;
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE workspaces SET name = $2, updated_at = now() WHERE id = $1")
        .bind(ws)
        .bind(&name)
        .execute(&mut *tx)
        .await?;
    Entry::new("workspace.update", &meta)
        .actor(&actor)
        .workspace(ws)
        .target("workspace", ws)
        .metadata(json!({ "name": name }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    fetch(&state, ws).await.map(Json)
}

pub async fn delete(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<serde_json::Value>> {
    let ws = grant.workspace_id()?;
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(ws)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM data_keys WHERE scope = $1")
        .bind(ws)
        .execute(&mut *tx)
        .await?;
    Entry::new("workspace.delete", &meta)
        .actor(&actor)
        .workspace(ws)
        .target("workspace", ws)
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    state.keys.forget(ws);
    Ok(Json(json!({ "ok": true })))
}
