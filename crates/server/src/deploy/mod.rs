//! Services, variables and deployments: everything between an API request and
//! the desired state a node reconciles to.

pub mod build_output;
pub mod config;
pub mod desired;
pub mod env;
pub mod placement;
pub mod start;
pub mod state;

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

macro_rules! remove_live_sql {
    ($cond:literal) => {
        concat!(
            "UPDATE deployments d SET status = 'removed', finished_at = now(), updated_at = now()
             FROM service_instances i, services s
             WHERE i.id = d.service_instance_id AND s.id = i.service_id
               AND d.status IN ('queued', 'building', 'deploying', 'active', 'crashed') AND ",
            $cond,
            " RETURNING d.node_id"
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
