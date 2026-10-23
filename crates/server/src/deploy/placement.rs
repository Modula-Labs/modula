//! Which node runs a service instance. Single-node for now: sticky to the
//! instance's node while it exists, else the least-loaded usable node.

use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{AppError, AppResult};

pub async fn choose(tx: &mut PgConnection, instance: Uuid) -> AppResult<Uuid> {
    let sticky: Option<Uuid> = sqlx::query_scalar(
        "SELECT n.id FROM service_instances i JOIN nodes n ON n.id = i.node_id
         WHERE i.id = $1 AND n.status <> 'revoked'",
    )
    .bind(instance)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(n) = sticky {
        return Ok(n);
    }
    // Prefer online nodes, then the fewest live deployments.
    let picked: Option<Uuid> = sqlx::query_scalar(
        "SELECT n.id FROM nodes n
         LEFT JOIN deployments d ON d.node_id = n.id AND d.status IN ('deploying', 'active', 'crashed')
         WHERE n.status <> 'revoked'
         GROUP BY n.id, n.status
         ORDER BY (n.status = 'online') DESC, count(d.id), n.created_at
         LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await?;
    let node = picked
        .ok_or_else(|| AppError::Conflict("no servers available: add a server first".into()))?;
    sqlx::query("UPDATE service_instances SET node_id = $2, updated_at = now() WHERE id = $1")
        .bind(instance)
        .bind(node)
        .execute(&mut *tx)
        .await?;
    Ok(node)
}
