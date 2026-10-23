//! Asking nodes to change how they reach (or serve) the control plane, over
//! their existing stream: `AccessConfig` out, `AccessStatus` back.

use std::time::Duration;

use futures::future::BoxFuture;
use modula_id::Id;
use modula_proto::node::v1 as pb;
use tokio::sync::oneshot;

use crate::state::AppState;

/// Installing cloudflared and checking the new route can take a while.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(300);

/// The capability a node advertises when it can apply an `AccessConfig`.
pub const CAPABILITY: &str = "cf-access";
/// The capability of the node that serves the dashboard (the main server).
pub const DASHBOARD_CAPABILITY: &str = "dashboard";

pub trait Agent: Send + Sync {
    /// Apply `cfg` on `node`; `Err` carries the node's (or the stream's) reason.
    fn apply<'a>(
        &'a self,
        state: &'a AppState,
        node: Id,
        cfg: pb::AccessConfig,
    ) -> BoxFuture<'a, Result<(), String>>;
}

/// Through the hub to the real node.
pub struct HubAgent;

impl Agent for HubAgent {
    fn apply<'a>(
        &'a self,
        state: &'a AppState,
        node: Id,
        mut cfg: pb::AccessConfig,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let request_id = Id::new().to_string();
            cfg.request_id = request_id.clone();
            let (tx, rx) = oneshot::channel();
            state
                .private
                .pending
                .lock()
                .expect("private lock")
                .insert(request_id.clone(), (node, tx));
            let sent = state
                .hub
                .send(node, pb::server_message::Msg::Access(cfg))
                .await;
            let outcome = if !sent {
                Err("the server is not connected".to_owned())
            } else {
                match tokio::time::timeout(ANSWER_TIMEOUT, rx).await {
                    Ok(Ok(s)) if s.ok => Ok(()),
                    Ok(Ok(s)) => Err(s.error),
                    Ok(Err(_)) => Err("the server disconnected before answering".to_owned()),
                    Err(_) => Err("the server did not answer in time".to_owned()),
                }
            };
            state
                .private
                .pending
                .lock()
                .expect("private lock")
                .remove(&request_id);
            outcome
        })
    }
}

/// A node as private mode sees it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NodeRow {
    pub id: Id,
    pub name: String,
    pub status: String,
    pub dashboard: bool,
    pub can_access: bool,
}

/// Every node that is not revoked.
pub async fn list(db: &sqlx::PgPool) -> sqlx::Result<Vec<NodeRow>> {
    sqlx::query_as(
        "SELECT id, name, status,
                coalesce(info->'capabilities' ? 'dashboard', false) AS dashboard,
                coalesce(info->'capabilities' ? 'cf-access', false) AS can_access
         FROM nodes WHERE status <> 'revoked' ORDER BY created_at",
    )
    .fetch_all(db)
    .await
}
