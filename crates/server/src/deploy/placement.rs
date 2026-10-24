//! Which nodes run a service instance. Replica 0 is sticky to the instance's
//! node while it exists, else the least-loaded usable node. More replicas go
//! one per server (anti-affinity): other online servers in the private
//! network, preferring those that ran the previous deployment's replicas,
//! then the least loaded. Only shared servers and the instance's own
//! workspace's servers are usable.

use modula_id::Id;
use sqlx::PgConnection;

use crate::error::{AppError, AppResult};

pub async fn choose(tx: &mut PgConnection, instance: Id) -> AppResult<Id> {
    let sticky: Option<Id> = sqlx::query_scalar(
        "SELECT n.id FROM service_instances i
         JOIN services s ON s.id = i.service_id
         JOIN projects p ON p.id = s.project_id
         JOIN nodes n ON n.id = i.node_id
         WHERE i.id = $1 AND n.status <> 'revoked'
           AND (n.workspace_id IS NULL OR n.workspace_id = p.workspace_id)",
    )
    .bind(instance)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(n) = sticky {
        return Ok(n);
    }
    // Prefer online nodes, then the fewest live deployments.
    let picked: Option<Id> = sqlx::query_scalar(
        "SELECT n.id FROM nodes n
         LEFT JOIN deployments d ON d.node_id = n.id AND d.status IN ('building', 'deploying', 'active', 'sleeping', 'crashed')
         WHERE n.status <> 'revoked'
           AND (n.workspace_id IS NULL OR n.workspace_id = (
                 SELECT p.workspace_id FROM service_instances i
                 JOIN services s ON s.id = i.service_id
                 JOIN projects p ON p.id = s.project_id
                 WHERE i.id = $1))
         GROUP BY n.id, n.status
         ORDER BY (n.status = 'online') DESC, count(d.id), n.created_at
         LIMIT 1",
    )
    .bind(instance)
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

/// A server that could run another replica.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Candidate {
    pub id: Id,
    pub name: String,
    pub online: bool,
    /// Live replicas it runs.
    pub load: i64,
    /// It runs a replica of the instance's serving deployment.
    pub previous: bool,
}

/// `n` distinct servers from `candidates`: previous ones first, then online
/// before offline, then the least loaded. None when there are too few online.
pub fn pick(candidates: &[Candidate], n: usize) -> Option<Vec<Id>> {
    let mut c: Vec<&Candidate> = candidates.iter().filter(|c| c.online).collect();
    c.sort_by(|a, b| {
        (!a.previous, a.load, &a.name, a.id).cmp(&(!b.previous, b.load, &b.name, b.id))
    });
    c.dedup_by_key(|c| c.id);
    (c.len() >= n).then(|| c.into_iter().take(n).map(|c| c.id).collect())
}

/// Servers for replicas 1… of `instance` (replica 0 runs on `primary`).
pub async fn choose_replicas(
    tx: &mut PgConnection,
    instance: Id,
    primary: Id,
    replicas: u32,
) -> AppResult<Vec<Id>> {
    let extra = replicas.saturating_sub(1) as usize;
    if extra == 0 {
        return Ok(Vec::new());
    }
    let in_mesh: bool = sqlx::query_scalar(
        "SELECT mesh_index IS NOT NULL AND wg_public_key IS NOT NULL FROM nodes WHERE id = $1",
    )
    .bind(primary)
    .fetch_one(&mut *tx)
    .await?;
    if !in_mesh {
        return Err(AppError::Conflict(
            "replicas on several servers need the private network, and this service's server \
             isn't in it yet (it needs a modula-node with WireGuard)"
                .into(),
        ));
    }
    let candidates: Vec<Candidate> = sqlx::query_as(
        "SELECT n.id, n.name, n.status = 'online' AS online,
                count(d.id) AS load,
                EXISTS (SELECT 1 FROM deployment_replicas pr
                        JOIN deployments pd ON pd.id = pr.deployment_id
                        WHERE pr.node_id = n.id AND pd.service_instance_id = $1
                          AND pd.status IN ('active', 'sleeping', 'crashed')) AS previous
         FROM nodes n
         LEFT JOIN deployment_replicas r ON r.node_id = n.id
         LEFT JOIN deployments d ON d.id = r.deployment_id
              AND d.status IN ('building', 'deploying', 'active', 'sleeping', 'crashed')
         WHERE n.status <> 'revoked' AND n.id <> $2
           AND n.mesh_index IS NOT NULL AND n.wg_public_key IS NOT NULL
           AND (n.workspace_id IS NULL OR n.workspace_id = (
                 SELECT p.workspace_id FROM service_instances i
                 JOIN services s ON s.id = i.service_id
                 JOIN projects p ON p.id = s.project_id
                 WHERE i.id = $1))
         GROUP BY n.id",
    )
    .bind(instance)
    .bind(primary)
    .fetch_all(&mut *tx)
    .await?;
    pick(&candidates, extra).ok_or_else(|| {
        let online = candidates.iter().filter(|c| c.online).count() + 1;
        AppError::Conflict(format!(
            "{replicas} replicas need {replicas} online servers in the private network \
             (one replica per server); {online} available. Add servers, or lower the \
             replicas and raise CPU and memory instead"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(name: &str, online: bool, load: i64, previous: bool) -> Candidate {
        Candidate {
            id: Id::new(),
            name: name.into(),
            online,
            load,
            previous,
        }
    }

    #[test]
    fn one_replica_per_server_least_loaded_first() {
        let (a, b, d) = (
            c("a", true, 5, false),
            c("b", true, 1, false),
            c("d", true, 3, false),
        );
        let off = c("off", false, 0, false);
        let all = [a.clone(), b.clone(), d.clone(), off];
        assert_eq!(pick(&all, 2), Some(vec![b.id, d.id]));
        assert_eq!(pick(&all, 3), Some(vec![b.id, d.id, a.id]));
        // Offline servers never get one: too few.
        assert_eq!(pick(&all, 4), None);
        assert_eq!(pick(&all, 0), Some(vec![]));
    }

    #[test]
    fn replicas_stay_where_they_ran() {
        let busy_previous = c("x", true, 9, true);
        let idle = c("y", true, 0, false);
        assert_eq!(
            pick(&[idle.clone(), busy_previous.clone()], 1),
            Some(vec![busy_previous.id])
        );
        assert_eq!(
            pick(&[idle.clone(), busy_previous.clone()], 2)
                .unwrap()
                .len(),
            2
        );
    }
}
