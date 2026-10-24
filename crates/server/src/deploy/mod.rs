//! Services, variables and deployments: everything between an API request and
//! the desired state a node reconciles to.

pub mod build_output;
pub mod config;
pub mod desired;
pub mod edge;
pub mod env;
pub mod output;
pub mod placement;
pub mod refs;
pub mod start;
pub mod state;
pub mod upload;

use modula_id::Id;
use sqlx::PgExecutor;

use crate::error::AppResult;

/// Mark a node's desired state as changed. The hub pushes the new snapshot.
pub async fn bump<'e>(db: impl PgExecutor<'e>, node_id: Id) -> AppResult<()> {
    sqlx::query("UPDATE nodes SET desired_generation = desired_generation + 1 WHERE id = $1")
        .bind(node_id)
        .execute(db)
        .await?;
    Ok(())
}

/// Stop every live deployment matched by `filter` for `id`, bumping the nodes
/// they ran on. Returns those nodes so the caller can kick the hub after commit.
pub async fn remove_live(
    tx: &mut sqlx::PgConnection,
    filter: LiveFilter,
    id: Id,
) -> AppResult<Vec<Id>> {
    let nodes: Vec<Option<Id>> = sqlx::query_scalar(filter.sql())
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
    let mut nodes: Vec<Id> = nodes.into_iter().flatten().collect();
    nodes.sort();
    nodes.dedup();
    for n in &nodes {
        bump(&mut *tx, *n).await?;
    }
    Ok(nodes)
}

#[derive(Debug, Clone, Copy)]
pub enum LiveFilter {
    Deployment,
    Service,
    Environment,
    Project,
    Node,
}

/// Returns the nodes that ran a replica of what was removed, and every node
/// running something else of the same environments (their address books).
macro_rules! remove_live_sql {
    ($cond:literal) => {
        concat!(
            "WITH gone AS (
                 UPDATE deployments d SET status = 'removed', finished_at = now(), updated_at = now()
                 FROM service_instances i, services s
                 WHERE i.id = d.service_instance_id AND s.id = i.service_id
                   AND d.status IN ('waiting_ci', 'queued', 'building', 'deploying', 'active', 'sleeping', 'crashed') AND ",
            $cond,
            " RETURNING d.id, i.environment_id)
             SELECT r.node_id FROM deployment_replicas r JOIN gone ON gone.id = r.deployment_id
             UNION
             SELECT r.node_id FROM deployment_replicas r
             JOIN deployments d ON d.id = r.deployment_id
             JOIN service_instances i ON i.id = d.service_instance_id
             WHERE i.environment_id IN (SELECT environment_id FROM gone)
               AND d.status IN ('building', 'deploying', 'active', 'sleeping', 'crashed')"
        )
    };
}

impl LiveFilter {
    fn sql(self) -> &'static str {
        match self {
            Self::Deployment => remove_live_sql!("d.id = $1"),
            Self::Service => remove_live_sql!("s.id = $1"),
            Self::Environment => remove_live_sql!("i.environment_id = $1"),
            Self::Project => remove_live_sql!("s.project_id = $1"),
            Self::Node => remove_live_sql!("d.node_id = $1"),
        }
    }
}

/// Before deleting a service, environment or project: queue its volumes'
/// data for deletion on their nodes (the rows go with the cascade). Returns
/// the nodes to kick after commit.
pub async fn queue_volume_removals(
    tx: &mut sqlx::PgConnection,
    filter: LiveFilter,
    id: Id,
) -> AppResult<Vec<Id>> {
    let sql = match filter {
        LiveFilter::Service => volume_removals_sql!("s.id = $1"),
        LiveFilter::Environment => volume_removals_sql!("i.environment_id = $1"),
        LiveFilter::Project => volume_removals_sql!("s.project_id = $1"),
        LiveFilter::Deployment | LiveFilter::Node => return Ok(Vec::new()),
    };
    let mut nodes: Vec<Id> = sqlx::query_scalar(sql).bind(id).fetch_all(&mut *tx).await?;
    nodes.sort();
    nodes.dedup();
    for n in &nodes {
        bump(&mut *tx, *n).await?;
    }
    Ok(nodes)
}

macro_rules! volume_removals_sql {
    ($cond:literal) => {
        concat!(
            "INSERT INTO volume_removals (volume_id, node_id)
             SELECT v.id, i.node_id FROM volumes v
             JOIN service_instances i ON i.id = v.service_instance_id
             JOIN services s ON s.id = i.service_id
             WHERE i.node_id IS NOT NULL AND ",
            $cond,
            " ON CONFLICT DO NOTHING RETURNING node_id"
        )
    };
}
use volume_removals_sql;
