//! Which nodes run a service instance. Replica 0 is sticky to the instance's
//! node while it exists, else a new one ([`rank`]): online, off the main
//! server when another is usable, close to the services it references (or
//! to the rest of its environment), then the least loaded. More replicas go
//! one per server (anti-affinity): other online servers in the private
//! network, preferring those that ran the previous deployment's replicas,
//! then the least loaded. Only shared servers and the instance's own
//! workspace's servers are usable. A server being migrated away (or already
//! migrated, awaiting deletion) gets nothing new.

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
    let environment: Id =
        sqlx::query_scalar("SELECT environment_id FROM service_instances WHERE id = $1")
            .bind(instance)
            .fetch_one(&mut *tx)
            .await?;
    let node = suggest(&mut *tx, environment, Some(instance), &[])
        .await?
        .first()
        .map(|s| s.id)
        .ok_or_else(|| AppError::Conflict("no servers available: add a server first".into()))?;
    sqlx::query("UPDATE service_instances SET node_id = $2, updated_at = now() WHERE id = $1")
        .bind(instance)
        .bind(node)
        .execute(&mut *tx)
        .await?;
    Ok(node)
}

/// A server a new service instance could start on.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Spot {
    pub id: Id,
    pub name: String,
    pub online: bool,
    /// Runs the control plane (the dashboard).
    pub main: bool,
    /// Catalog location id (locations.rs).
    pub location: Option<String>,
    /// Live deployments it runs.
    pub load: i64,
}

/// Where a new service should be close to: a server that runs a service it
/// references, or where most of its environment runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Anchor {
    pub node: Id,
    pub location: Option<String>,
}

/// Distance for a server whose location (or the anchor's) is unknown.
const FAR_KM: f64 = 20_000.0;

fn distance_km(spot: &Spot, a: &Anchor) -> f64 {
    if spot.id == a.node {
        return 0.0;
    }
    match (spot.location.as_deref(), a.location.as_deref()) {
        (Some(x), Some(y)) if x == y => 0.0,
        (Some(x), Some(y)) => match (crate::locations::get(x), crate::locations::get(y)) {
            (Some(p), Some(q)) => crate::locations::distance_km(p.lat, p.lon, q.lat, q.lon),
            _ => FAR_KM,
        },
        _ => FAR_KM,
    }
}

/// Distance to the nearest anchor in 100 km steps, so servers in one metro
/// area tie and fall through to load.
fn closeness(spot: &Spot, anchors: &[Anchor]) -> i64 {
    anchors
        .iter()
        .map(|a| distance_km(spot, a))
        .min_by(f64::total_cmp)
        .map_or(0, |d| (d / 100.0).round() as i64)
}

/// `spots` best first for a new service: online ones, then off the main
/// server (it runs the control plane), then nearest to `anchors`, then the
/// least loaded.
pub fn rank(spots: &[Spot], anchors: &[Anchor]) -> Vec<Spot> {
    let mut v = spots.to_vec();
    v.sort_by_cached_key(|s| {
        (
            !s.online,
            s.main,
            closeness(s, anchors),
            s.load,
            s.name.clone(),
            s.id,
        )
    });
    v
}

/// The servers a new service in `environment` could use (shared ones and
/// its workspace's; none being migrated away).
async fn spots(conn: &mut PgConnection, environment: Id) -> AppResult<Vec<Spot>> {
    Ok(sqlx::query_as(
        "SELECT n.id, n.name, n.status = 'online' AS online,
                coalesce(n.info->'capabilities' ? 'dashboard', false) AS main,
                n.location, count(d.id) AS load
         FROM nodes n
         LEFT JOIN deployments d ON d.node_id = n.id
              AND d.status IN ('building', 'deploying', 'active', 'sleeping', 'crashed')
         WHERE n.status <> 'revoked'
           AND NOT EXISTS (SELECT 1 FROM server_migrations m
                           WHERE m.source_node_id = n.id AND m.status IN ('running', 'done'))
           AND (n.workspace_id IS NULL OR n.workspace_id = (
                 SELECT p.workspace_id FROM environments e
                 JOIN projects p ON p.id = e.project_id WHERE e.id = $1))
         GROUP BY n.id",
    )
    .bind(environment)
    .fetch_all(&mut *conn)
    .await?)
}

/// The servers of the services `instance` references through its variables
/// (plus `references`, service names); without any placed, those where most
/// of the environment's other services run.
async fn anchors(
    conn: &mut PgConnection,
    environment: Id,
    instance: Option<Id>,
    references: &[String],
) -> AppResult<Vec<Anchor>> {
    use crate::deploy::refs::{Ref, Target};
    let mut names: Vec<String> = references.iter().map(|s| s.to_ascii_lowercase()).collect();
    if let Some(i) = instance {
        let refs: Vec<String> =
            sqlx::query_scalar("SELECT unnest(refs) FROM variables WHERE service_instance_id = $1")
                .bind(i)
                .fetch_all(&mut *conn)
                .await?;
        names.extend(refs.iter().filter_map(|r| match Ref::parse(r)?.target {
            Target::Service(s) => Some(s),
            _ => None,
        }));
    }
    let placed: Vec<(Id, Option<String>, String)> = sqlx::query_as(
        "SELECT n.id, n.location, s.name FROM service_instances i
         JOIN services s ON s.id = i.service_id
         JOIN nodes n ON n.id = i.node_id
         WHERE i.environment_id = $1 AND i.id IS DISTINCT FROM $2 AND n.status <> 'revoked'",
    )
    .bind(environment)
    .bind(instance)
    .fetch_all(&mut *conn)
    .await?;
    let anchor = |(node, location, _): &(Id, Option<String>, String)| Anchor {
        node: *node,
        location: location.clone(),
    };
    let referenced: Vec<Anchor> = placed
        .iter()
        .filter(|p| names.contains(&p.2))
        .map(anchor)
        .collect();
    if !referenced.is_empty() {
        return Ok(referenced);
    }
    // The majority location (a server without one counts as its own place).
    let key = |p: &(Id, Option<String>, String)| p.1.clone().unwrap_or_else(|| p.0.to_string());
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for p in &placed {
        *counts.entry(key(p)).or_default() += 1;
    }
    let Some(top) = counts
        .iter()
        .max_by_key(|(_, n)| **n)
        .map(|(k, _)| k.clone())
    else {
        return Ok(Vec::new());
    };
    Ok(placed
        .iter()
        .filter(|p| key(p) == top)
        .map(anchor)
        .collect())
}

/// Servers for a new service in `environment`, best first (see [`rank`]).
/// `instance` is the service's own instance when it exists already (its
/// variables' references count, and it's not its own neighbour).
pub async fn suggest(
    conn: &mut PgConnection,
    environment: Id,
    instance: Option<Id>,
    references: &[String],
) -> AppResult<Vec<Spot>> {
    let spots = spots(&mut *conn, environment).await?;
    let anchors = anchors(&mut *conn, environment, instance, references).await?;
    Ok(rank(&spots, &anchors))
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
           AND NOT EXISTS (SELECT 1 FROM server_migrations m
                           WHERE m.source_node_id = n.id AND m.status IN ('running', 'done'))
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

    fn spot(name: &str, main: bool, location: Option<&str>, load: i64) -> Spot {
        Spot {
            id: Id::new(),
            name: name.into(),
            online: true,
            main,
            location: location.map(Into::into),
            load,
        }
    }

    #[test]
    fn new_services_prefer_a_worker_close_to_their_references() {
        let main = spot("main", true, Some("hetzner:fsn1"), 0);
        let far = spot("far", false, Some("hetzner:ash"), 0);
        let near = spot("near", false, Some("hetzner:nbg1"), 5);
        let all = [main.clone(), far.clone(), near.clone()];
        // No anchors: off the main server, then least loaded.
        assert_eq!(rank(&all, &[])[0].id, far.id);
        assert_eq!(rank(&all, &[]).last().unwrap().id, main.id);
        // Near the referenced database (on the main server, in Falkenstein).
        let db = Anchor {
            node: main.id,
            location: main.location.clone(),
        };
        let ranked: Vec<Id> = rank(&all, std::slice::from_ref(&db))
            .iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ranked, vec![near.id, far.id, main.id]);
        // The main server only when nothing else is online.
        let mut off = far.clone();
        off.online = false;
        assert_eq!(rank(&[off.clone(), main.clone()], &[])[0].id, main.id);
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
