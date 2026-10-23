//! A project's activity feed: change sets and deployments, newest first.

use axum::Json;
use axum::extract::{Query, State};
use modula_id::Id;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::authz::Grant;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

const MAX_GROUPS: usize = 50;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActivityQuery {
    pub environment_id: Option<Id>,
    pub service_id: Option<Id>,
    /// `deployment`, `variable`, `config` or `service`; all when absent.
    pub kind: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    pub items: Vec<Group>,
}

/// One request's changes, or one deployment.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub id: Id,
    pub service_id: Option<Id>,
    pub service_name: Option<String>,
    pub actor: Option<Actor>,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub changes: Vec<Change>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Actor {
    pub name: String,
    pub avatar_url: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub kind: String,
    pub op: String,
    pub key: Option<String>,
    pub status: Option<String>,
    pub deployment_id: Option<Id>,
    pub message: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct ChangeRow {
    set_id: Id,
    service_id: Option<Id>,
    service_name: Option<String>,
    actor: Option<String>,
    kind: String,
    op: String,
    key: Option<String>,
    at: OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct DeploymentRow {
    id: Id,
    service_id: Id,
    service_name: String,
    actor: Option<String>,
    avatar_url: Option<String>,
    status: String,
    commit_message: Option<String>,
    at: OffsetDateTime,
}

pub async fn list(
    State(state): State<AppState>,
    grant: Grant,
    Query(q): Query<ActivityQuery>,
) -> AppResult<Json<Activity>> {
    let project = grant.id();
    let kind = q.kind.as_deref();
    if !matches!(
        kind,
        None | Some("deployment" | "variable" | "config" | "service")
    ) {
        return Err(AppError::bad_request(
            "kind must be deployment, variable, config or service",
        ));
    }
    let mut items = Vec::new();

    if kind != Some("deployment") {
        let rows: Vec<ChangeRow> = sqlx::query_as(
            "SELECT cs.id AS set_id, c.service_id, s.name AS service_name, u.name AS actor,
                    c.kind, c.op, c.key, c.created_at AS at
             FROM change_sets cs
             JOIN environments e ON e.id = cs.environment_id
             JOIN changes c ON c.change_set_id = cs.id
             LEFT JOIN services s ON s.id = c.service_id
             LEFT JOIN users u ON u.id = cs.created_by
             WHERE e.project_id = $1 AND cs.status = 'applied'
               AND ($2::text IS NULL OR cs.environment_id = $2)
               AND ($3::text IS NULL OR c.service_id = $3)
               AND ($4::text IS NULL OR c.kind = $4)
             ORDER BY cs.created_at DESC, cs.id DESC, c.id
             LIMIT 1000",
        )
        .bind(project)
        .bind(q.environment_id)
        .bind(q.service_id)
        .bind(kind)
        .fetch_all(&state.db)
        .await?;
        for r in rows {
            let change = Change {
                kind: r.kind,
                op: r.op,
                key: r.key,
                status: None,
                deployment_id: None,
                message: None,
                at: r.at,
            };
            match items.last_mut() {
                Some(Group { id, changes, .. }) if *id == r.set_id => changes.push(change),
                _ => items.push(Group {
                    id: r.set_id,
                    service_id: r.service_id,
                    service_name: r.service_name,
                    actor: r.actor.map(|name| Actor {
                        name,
                        avatar_url: None,
                    }),
                    at: r.at,
                    changes: vec![change],
                }),
            }
        }
        items.truncate(MAX_GROUPS);
    }

    if matches!(kind, None | Some("deployment")) {
        let rows: Vec<DeploymentRow> = sqlx::query_as(
            "SELECT d.id, s.id AS service_id, s.name AS service_name,
                    coalesce(u.name, nullif(d.commit_author, '')) AS actor,
                    CASE WHEN d.created_by IS NULL AND d.commit_author_login IS NOT NULL
                         THEN 'https://github.com/' || d.commit_author_login || '.png?size=64' END AS avatar_url,
                    d.status, d.commit_message, d.updated_at AS at
             FROM deployments d
             JOIN service_instances i ON i.id = d.service_instance_id
             JOIN services s ON s.id = i.service_id
             LEFT JOIN users u ON u.id = d.created_by
             WHERE s.project_id = $1
               AND ($2::text IS NULL OR i.environment_id = $2)
               AND ($3::text IS NULL OR s.id = $3)
             ORDER BY d.created_at DESC
             LIMIT 50",
        )
        .bind(project)
        .bind(q.environment_id)
        .bind(q.service_id)
        .fetch_all(&state.db)
        .await?;
        items.extend(rows.into_iter().map(|r| Group {
            id: r.id,
            service_id: Some(r.service_id),
            service_name: Some(r.service_name),
            actor: r.actor.map(|name| Actor {
                name,
                avatar_url: r.avatar_url,
            }),
            at: r.at,
            changes: vec![Change {
                kind: "deployment".into(),
                op: "status".into(),
                key: None,
                status: Some(r.status),
                deployment_id: Some(r.id),
                message: r.commit_message,
                at: r.at,
            }],
        }));
    }

    items.sort_by_key(|g| std::cmp::Reverse(g.at));
    items.truncate(MAX_GROUPS);
    Ok(Json(Activity { items }))
}
