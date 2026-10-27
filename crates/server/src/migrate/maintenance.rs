//! While the main server's control plane moves (migrate/promote.rs) the
//! dashboard is read-only: writes get 503 with a message, and only the
//! migration's own jobs run (anything else would change a database that is
//! about to be copied, or dispatch work whose outcome the copy never learns
//! about). Once the new control plane took over, the old one is *moved*: its
//! API answers 503 with the new address, page loads redirect there, and no
//! job runs at all.
//!
//! Derived from `server_migrations` ([`sync`]) at startup and after every
//! change, and kept in memory for the request path.

use std::sync::RwLock;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use modula_id::Id;
use serde::Serialize;
use serde_json::json;

use crate::error::AppResult;
use crate::state::AppState;

pub const MESSAGE: &str = "The dashboard is moving to a new server: changes are paused for a few minutes. Your apps keep running.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Read-only while the control plane is copied and handed over.
    Maintenance { new_url: Option<String> },
    /// This control plane handed over to the one at `to` (its node endpoint
    /// at `node_url`); `source_node` is this server's own node.
    Moved {
        to: String,
        node_url: String,
        source_node: Option<Id>,
    },
}

#[derive(Debug, Default)]
pub struct Maintenance(RwLock<Option<Mode>>);

impl Maintenance {
    pub fn get(&self) -> Option<Mode> {
        crate::sync::read(&self.0).clone()
    }

    pub fn set(&self, mode: Option<Mode>) {
        *crate::sync::write(&self.0) = mode;
    }

    pub fn is_on(&self) -> bool {
        self.get().is_some()
    }

    /// Job kinds that may run now (None: all).
    pub fn job_kinds(&self) -> Option<&'static [&'static str]> {
        match self.get() {
            None => None,
            Some(Mode::Maintenance { .. }) => {
                Some(&[super::server::MIGRATE_JOB, super::moves::MOVE_JOB])
            }
            Some(Mode::Moved { .. }) => Some(&[]),
        }
    }
}

/// Re-derive the mode from the database.
pub async fn sync(state: &AppState) -> AppResult<()> {
    let moved: Option<(Option<String>, Option<String>, Option<Id>)> = sqlx::query_as(
        "SELECT new_public_url, new_node_url, source_node_id FROM server_migrations
         WHERE retired_at IS NOT NULL ORDER BY retired_at DESC LIMIT 1",
    )
    .fetch_optional(&state.db)
    .await?;
    let mode = match moved {
        Some((to, node_url, source_node)) => Some(Mode::Moved {
            to: to.unwrap_or_default(),
            node_url: node_url.unwrap_or_default(),
            source_node,
        }),
        None => {
            let running: Option<Option<String>> = sqlx::query_scalar(
                "SELECT new_public_url FROM server_migrations
                 WHERE main AND status = 'running' AND step = 'promote' LIMIT 1",
            )
            .fetch_optional(&state.db)
            .await?;
            running.map(|new_url| Mode::Maintenance { new_url })
        }
    };
    if mode != state.maintenance.get() {
        tracing::info!(?mode, "maintenance mode changed");
    }
    state.maintenance.set(mode);
    Ok(())
}

fn refused(code: &str, message: &str, moved_to: Option<&str>) -> Response {
    let mut error = json!({ "code": code, "message": message });
    if let Some(to) = moved_to {
        error["movedTo"] = json!(to);
    }
    let mut res = (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": error })),
    )
        .into_response();
    res.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("10"));
    res
}

/// `POST /api/server-migrations/{id}/abort`: allowed during maintenance.
fn is_abort(method: &Method, path: &str) -> bool {
    method == Method::POST
        && path
            .strip_prefix("/api/server-migrations/")
            .and_then(|r| r.strip_suffix("/abort"))
            .is_some_and(|id| !id.is_empty() && !id.contains('/'))
}

fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
}

/// The request gate (outermost API layer).
pub async fn gate(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(mode) = state.maintenance.get() else {
        return next.run(req).await;
    };
    let path = req.uri().path().to_owned();
    if path == "/api/maintenance" {
        return next.run(req).await;
    }
    let read = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    match mode {
        Mode::Maintenance { .. } => {
            if read || path.starts_with("/api/auth/") || is_abort(req.method(), &path) {
                next.run(req).await
            } else {
                refused("maintenance", MESSAGE, None)
            }
        }
        Mode::Moved { to, .. } => {
            let message = format!("This dashboard moved to {to}.");
            if path.starts_with("/api/") || !read {
                return refused("moved", &message, Some(&to));
            }
            // A page: go there, unless "there" is this very name (a domain
            // whose DNS still points here): then the page itself waits.
            let here = req
                .headers()
                .get(header::HOST)
                .and_then(|h| h.to_str().ok())
                .map(|h| h.split(':').next().unwrap_or(h).to_ascii_lowercase());
            if here.is_some() && here == host_of(&to) {
                return next.run(req).await;
            }
            let target = format!(
                "{}{}",
                to.trim_end_matches('/'),
                req.uri().path_and_query().map_or("/", |p| p.as_str())
            );
            match HeaderValue::from_str(&target) {
                Ok(v) => (StatusCode::TEMPORARY_REDIRECT, [(header::LOCATION, v)]).into_response(),
                Err(_) => refused("moved", &message, Some(&to)),
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// `maintenance`, `moved`, or None.
    pub mode: Option<&'static str>,
    pub message: Option<String>,
    /// Where the dashboard is (moved) or will be (maintenance).
    pub new_url: Option<String>,
}

/// `GET /api/maintenance` (public): what the dashboard shows full-screen.
pub async fn status(State(state): State<AppState>) -> Json<Status> {
    Json(match state.maintenance.get() {
        None => Status {
            mode: None,
            message: None,
            new_url: None,
        },
        Some(Mode::Maintenance { new_url }) => Status {
            mode: Some("maintenance"),
            message: Some(MESSAGE.into()),
            new_url,
        },
        Some(Mode::Moved { to, .. }) => Status {
            mode: Some("moved"),
            message: Some(format!("This dashboard moved to {to}.")),
            new_url: Some(to),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_abort_endpoint_passes() {
        assert!(is_abort(&Method::POST, "/api/server-migrations/abc/abort"));
        assert!(!is_abort(&Method::GET, "/api/server-migrations/abc/abort"));
        assert!(!is_abort(&Method::POST, "/api/server-migrations//abort"));
        assert!(!is_abort(&Method::POST, "/api/server-migrations/a/b/abort"));
        assert!(!is_abort(&Method::POST, "/api/projects"));
    }
}
