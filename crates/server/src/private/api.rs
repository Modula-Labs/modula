//! `/api/instance/private-access`: status, a live readiness check, and the
//! switch. Instance admins only; turning it on or off also needs sudo and
//! admin rights in the workspace whose Cloudflare connection is used.

use axum::Json;
use axum::extract::{Query, State};
use modula_id::Id;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::nodes;
use super::orchestrate;
use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::{self, Action, Resource, ResourceKind};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::integrations::cloudflare as cf;
use crate::state::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// `off`, `enabling`, `on` or `disabling`.
    pub phase: String,
    pub dashboard_host: Option<String>,
    pub node_host: Option<String>,
    pub workspace_id: Option<Id>,
    pub team_domain: Option<String>,
    pub allowed_emails: Vec<String>,
    pub allowed_domains: Vec<String>,
    /// Progress of the current or last change: `{label, status, detail?}`.
    pub steps: Value,
    pub error: Option<String>,
    /// Break-glass access over SSH, e.g. `ssh -L 7100:127.0.0.1:7100 <server>`.
    pub break_glass: String,
}

async fn load(state: &AppState) -> AppResult<Status> {
    let s = super::settings(&state.db).await?;
    Ok(Status {
        phase: s.phase,
        dashboard_host: s
            .dashboard_host
            .or_else(|| state.cfg.public_url.host_str().map(str::to_owned)),
        node_host: s.node_host,
        workspace_id: s.workspace_id,
        team_domain: s.team_domain,
        allowed_emails: s.allowed_emails,
        allowed_domains: s.allowed_domains,
        steps: s.steps,
        error: s.error,
        break_glass: format!(
            "ssh -L {port}:127.0.0.1:{port} <main server>, then open http://localhost:{port}",
            port = state.cfg.listen.port()
        ),
    })
}

pub async fn get(State(state): State<AppState>) -> AppResult<Json<Status>> {
    Ok(Json(load(&state).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckQuery {
    pub workspace_id: Id,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckItem {
    pub label: String,
    pub ok: bool,
    pub detail: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub items: Vec<CheckItem>,
    /// The workspace admins' emails: the default allow list.
    pub suggested_emails: Vec<String>,
    pub ready: bool,
}

async fn workspace_admin(state: &AppState, actor: &Actor, workspace: Id) -> AppResult<()> {
    authz::authorize(
        &state.db,
        actor,
        Action::CloudflareConnect,
        Resource {
            kind: ResourceKind::Workspace,
            id: workspace,
        },
    )
    .await
    .map(|_| ())
}

/// The readiness checklist, asked live (Cloudflare and the servers).
pub async fn check(
    State(state): State<AppState>,
    actor: Actor,
    Query(q): Query<CheckQuery>,
) -> AppResult<Json<Check>> {
    workspace_admin(&state, &actor, q.workspace_id).await?;
    let mut items = vec![];
    let mut item = |label: &str, ok: bool, detail: Option<String>| {
        items.push(CheckItem {
            label: label.into(),
            ok,
            detail,
        });
    };
    let conn = cf::connection(&state, q.workspace_id).await?;
    item(
        "Cloudflare is connected to this workspace",
        conn.is_some(),
        None,
    );
    if let Some(conn) = &conn {
        match &conn.metadata.account_id {
            Some(acc) => {
                let tunnel = conn.client.tunnel_access(acc).await;
                let access = conn.client.access_apps_access(acc).await;
                let hint = |r: &Result<bool, cf::client::Error>, perm: &str| match r {
                    Ok(true) => None,
                    Ok(false) => Some(format!("Add {perm} to the token and replace it")),
                    Err(e) => Some(e.to_string()),
                };
                item(
                    "The token may manage tunnels",
                    matches!(tunnel, Ok(true)),
                    hint(&tunnel, "Account · Cloudflare Tunnel · Edit"),
                );
                item(
                    "The token may manage Access applications",
                    matches!(access, Ok(true)),
                    hint(&access, "Account · Access: Apps and Policies · Edit"),
                );
                let team = conn.client.team_domain(acc).await;
                item(
                    "Cloudflare Zero Trust is set up",
                    matches!(team, Ok(Some(_))),
                    match team {
                        Ok(Some(t)) => Some(t),
                        Ok(None) => Some(
                            "Open one.dash.cloudflare.com once and pick a team name (free)".into(),
                        ),
                        Err(e) => Some(e.to_string()),
                    },
                );
            }
            None => item(
                "The token can see a Cloudflare account",
                false,
                Some("Reconnect with an account-scoped token".into()),
            ),
        }
        if let Some(host) = state.cfg.public_url.host_str() {
            let zone = conn.client.find_zone(host).await.ok().flatten();
            item(
                &format!("{host} is in a zone the token manages"),
                zone.is_some(),
                zone.map(|z| z.name),
            );
        }
    }
    let all = nodes::list(&state.db).await?;
    let main = all.iter().find(|n| n.dashboard);
    item(
        "The main server is online and up to date",
        main.is_some_and(|m| m.status == "online" && m.can_access),
        main.map(|m| m.name.clone()),
    );
    let not_ready: Vec<String> = all
        .iter()
        .filter(|n| !n.dashboard && (n.status != "online" || !n.can_access))
        .map(|n| n.name.clone())
        .collect();
    item(
        "Every other server is online and up to date",
        not_ready.is_empty(),
        (!not_ready.is_empty()).then(|| not_ready.join(", ")),
    );
    let suggested_emails: Vec<String> = sqlx::query_scalar(
        "SELECT lower(u.email) FROM workspace_members m JOIN users u ON u.id = m.user_id
         WHERE m.workspace_id = $1 AND m.role = 'admin' ORDER BY u.email",
    )
    .bind(q.workspace_id)
    .fetch_all(&state.db)
    .await?;
    let ready = items.iter().all(|i| i.ok);
    Ok(Json(Check {
        items,
        suggested_emails,
        ready,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Update {
    pub enabled: bool,
    #[serde(default)]
    pub workspace_id: Option<Id>,
    #[serde(default)]
    pub node_host: Option<String>,
    #[serde(default)]
    pub allowed_emails: Vec<String>,
    #[serde(default)]
    pub allowed_domains: Vec<String>,
}

/// Starts turning private mode on or off; progress shows in the status.
pub async fn put(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
    Json(req): Json<Update>,
) -> AppResult<Json<Status>> {
    let current = super::settings(&state.db).await?;
    // Held only to check and start: the background task takes it for the work.
    let guard = state
        .private
        .busy
        .try_lock()
        .map_err(|_| AppError::Conflict("a private dashboard change is running".into()))?;
    if req.enabled {
        let workspace = req.workspace_id.ok_or_else(|| {
            AppError::bad_request("choose the workspace whose Cloudflare connection to use")
        })?;
        workspace_admin(&state, &actor, workspace).await?;
        let plan = orchestrate::preflight(
            &state,
            workspace,
            req.node_host.as_deref(),
            &req.allowed_emails,
            &req.allowed_domains,
        )
        .await?;
        Entry::new("instance.private_access.enable", &meta)
            .actor(&actor)
            .workspace(workspace)
            .metadata(json!({
                "dashboardHost": plan.dashboard_host,
                "nodeHost": plan.node_host,
                "emails": plan.emails,
                "domains": plan.domains,
            }))
            .record(&state.db)
            .await?;
        orchestrate::mark_enabling(&state, &plan, Some(actor.user_id)).await?;
        drop(guard);
        let (st, by) = (state.clone(), actor.user_id);
        tokio::spawn(async move {
            let _busy = st.private.busy.lock().await;
            let _ = orchestrate::enable(&st, plan, Some(by)).await;
        });
    } else {
        if current.phase != "on" {
            return Err(AppError::Conflict(format!(
                "private mode is {}",
                current.phase
            )));
        }
        if let Some(ws) = current.workspace_id {
            workspace_admin(&state, &actor, ws).await?;
        }
        Entry::new("instance.private_access.disable", &meta)
            .actor(&actor)
            .record(&state.db)
            .await?;
        sqlx::query(
            "UPDATE private_access SET phase = 'disabling', steps = '[]'::jsonb, error = NULL, updated_by = $1",
        )
            .bind(actor.user_id)
            .execute(&state.db)
            .await?;
        drop(guard);
        let st = state.clone();
        tokio::spawn(async move {
            let _busy = st.private.busy.lock().await;
            match orchestrate::teardown(&st, "turned off").await {
                Ok(w) if !w.is_empty() => {
                    let _ = sqlx::query("UPDATE private_access SET error = $1")
                        .bind(w.join("; "))
                        .execute(&st.db)
                        .await;
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = sqlx::query("UPDATE private_access SET error = $1")
                        .bind(format!("{e:#}"))
                        .execute(&st.db)
                        .await;
                }
            }
        });
    }
    Ok(Json(load(&state).await?))
}

/// The extra join arguments for new servers while private mode is on.
pub struct JoinAccess {
    pub server: String,
    pub host: String,
    pub client_id: String,
    pub client_secret: String,
}

pub async fn join_access(state: &AppState) -> AppResult<Option<JoinAccess>> {
    let s = super::settings(&state.db).await?;
    let (Some(ws), Some(host), Some(cid), Some(sealed)) = (
        s.workspace_id,
        s.node_host,
        s.node_token_client_id,
        s.node_token_secret_sealed,
    ) else {
        return Ok(None);
    };
    if s.phase != "on" {
        return Ok(None);
    }
    let secret = state
        .keys
        .open(
            &state.db,
            &sealed,
            super::aad(ws, "node_token_secret_sealed"),
        )
        .await?;
    Ok(Some(JoinAccess {
        server: orchestrate::tunneled_server(state),
        host,
        client_id: cid,
        client_secret: String::from_utf8(secret.to_vec())
            .map_err(|e| AppError::Internal(e.into()))?,
    }))
}

impl JoinAccess {
    /// Rewrites a join command (`… --server X …`) for private mode.
    pub fn apply(&self, command: &str, direct_server: &str) -> String {
        format!(
            "{} --access-host {} --access-id {} --access-secret {}",
            command.replace(
                &format!("--server {direct_server}"),
                &format!("--server {}", self.server)
            ),
            self.host,
            self.client_id,
            self.client_secret
        )
    }
}
