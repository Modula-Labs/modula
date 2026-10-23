//! "Add server" suggestions: when no existing server has room for a service,
//! the cheapest server type that fits in the preferred location. Only ever a
//! suggestion; servers are created by the user.

use axum::Json;
use axum::extract::{Query, State};
use modula_id::Id;
use serde::{Deserialize, Serialize};

use super::{ServerProvider, ServerTypeOffer};
use crate::authz::Grant;
use crate::error::{AppError, AppResult};
use crate::integrations::hetzner::{self, Caps};
use crate::locations;
use crate::state::AppState;

/// What a service without limits is assumed to need.
pub const DEFAULT_CPU_MILLIS: i64 = 250;
pub const DEFAULT_MEMORY_MB: i64 = 256;
/// Kept free on every server for the OS, Docker, Caddy and modula-node.
pub const SYSTEM_MEMORY_MB: i64 = 512;
/// Where to suggest a server when no location is given.
const DEFAULT_LOCATION: &str = "hetzner:fsn1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SuggestQuery {
    pub cpu_millis: Option<i64>,
    pub memory_mb: Option<i64>,
    /// Catalog id, e.g. `hetzner:fsn1`.
    pub location: Option<String>,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NodeRoom {
    pub node_id: Id,
    pub name: String,
    pub free_cpu_millis: i64,
    pub free_memory_mb: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Suggestion {
    pub provider: &'static str,
    pub location: &'static str,
    pub server_type: ServerTypeOffer,
    pub monthly_cents: i64,
    /// False when adding it would exceed the workspace's caps.
    pub within_caps: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Answer {
    /// Existing servers with room, most room first. Non-empty = no new server needed.
    pub fits: Vec<NodeRoom>,
    pub suggestion: Option<Suggestion>,
    /// Why there is no suggestion, when there is none and nothing fits.
    pub reason: Option<String>,
}

/// Free room per usable server: capacity from what the node reports, minus
/// the limits of every service instance placed on it.
pub async fn room(db: &sqlx::PgPool, location: Option<&str>) -> AppResult<Vec<NodeRoom>> {
    let rows: Vec<NodeRoom> = sqlx::query_as::<_, (Id, String, i64, i64)>(
        "SELECT n.id, n.name,
                (COALESCE((n.info->>'cpus')::bigint, 0) * 1000
                  - COALESCE(sum(COALESCE((i.config->>'cpuMillis')::bigint, $2)) FILTER (WHERE i.id IS NOT NULL), 0))::bigint,
                (COALESCE((n.info->>'memoryBytes')::bigint, 0) / 1048576 - $4
                  - COALESCE(sum(COALESCE((i.config->>'memoryMb')::bigint, $3)) FILTER (WHERE i.id IS NOT NULL), 0))::bigint
         FROM nodes n
         LEFT JOIN service_instances i ON i.node_id = n.id
         WHERE n.status <> 'revoked' AND ($1::text IS NULL OR n.location = $1)
         GROUP BY n.id
         ORDER BY n.created_at",
    )
    .bind(location)
    .bind(DEFAULT_CPU_MILLIS)
    .bind(DEFAULT_MEMORY_MB)
    .bind(SYSTEM_MEMORY_MB)
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|(node_id, name, cpu, mem)| NodeRoom {
        node_id,
        name,
        free_cpu_millis: cpu,
        free_memory_mb: mem,
    })
    .collect();
    Ok(rows)
}

/// The cheapest type in `location` with room for the request.
pub fn cheapest_fit<'a>(
    types: &'a [ServerTypeOffer],
    location: &str,
    cpu_millis: i64,
    memory_mb: i64,
) -> Option<(&'a ServerTypeOffer, i64)> {
    types
        .iter()
        .filter(|t| i64::from(t.cores) * 1000 >= cpu_millis)
        .filter(|t| (t.memory_gb * 1024.0) as i64 - SYSTEM_MEMORY_MB >= memory_mb)
        .filter_map(|t| t.price_in(location).map(|p| (t, p)))
        .min_by_key(|(t, p)| (*p, t.cores, t.name.clone()))
}

pub async fn suggest(
    State(state): State<AppState>,
    grant: Grant,
    Query(q): Query<SuggestQuery>,
) -> AppResult<Json<Answer>> {
    let workspace = grant.id();
    let cpu = q.cpu_millis.unwrap_or(DEFAULT_CPU_MILLIS);
    let mem = q.memory_mb.unwrap_or(DEFAULT_MEMORY_MB);
    if !(0..=1_000_000).contains(&cpu) || !(0..=10_000_000).contains(&mem) {
        return Err(AppError::bad_request(
            "cpuMillis or memoryMb is out of range",
        ));
    }
    let location = match q.location.as_deref() {
        Some(l) => Some(
            locations::get(l)
                .ok_or_else(|| AppError::bad_request("unknown location"))?
                .id,
        ),
        None => None,
    };

    let mut fits: Vec<NodeRoom> = room(&state.db, location)
        .await?
        .into_iter()
        .filter(|r| r.free_cpu_millis >= cpu && r.free_memory_mb >= mem)
        .collect();
    fits.sort_by_key(|r| std::cmp::Reverse((r.free_memory_mb, r.free_cpu_millis)));
    if !fits.is_empty() {
        return Ok(Json(Answer {
            fits,
            suggestion: None,
            reason: None,
        }));
    }

    let target = location.unwrap_or(DEFAULT_LOCATION);
    if !target.starts_with("hetzner:") {
        return Ok(Json(Answer {
            fits,
            suggestion: None,
            reason: Some("servers can only be created in Hetzner locations".into()),
        }));
    }
    let Some(provider) = hetzner::provider(&state, workspace).await? else {
        return Ok(Json(Answer {
            fits,
            suggestion: None,
            reason: Some("no server has room; connect Hetzner or add a server manually".into()),
        }));
    };
    let types = provider.server_types().await?;
    let Some((t, price)) = cheapest_fit(&types, target, cpu, mem) else {
        return Ok(Json(Answer {
            fits,
            suggestion: None,
            reason: Some("no Hetzner server type in this location is large enough".into()),
        }));
    };
    let metadata: serde_json::Value = sqlx::query_scalar(
        "SELECT metadata FROM integrations WHERE workspace_id = $1 AND kind = 'hetzner'",
    )
    .bind(workspace)
    .fetch_optional(&state.db)
    .await?
    .unwrap_or_default();
    let caps = Caps::from_metadata(&metadata);
    let used = hetzner::usage(&state.db, workspace).await?;
    let within_caps =
        used.servers < caps.max_servers && used.monthly_cents + price <= caps.max_monthly_cents;
    let location = locations::get(target)
        .map(|l| l.id)
        .unwrap_or(DEFAULT_LOCATION);
    Ok(Json(Answer {
        fits,
        suggestion: Some(Suggestion {
            provider: hetzner::KIND,
            location,
            server_type: t.clone(),
            monthly_cents: price,
            within_caps,
        }),
        reason: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provision::LocationPrice;

    fn offer(name: &str, cores: u32, memory_gb: f64, cents: i64) -> ServerTypeOffer {
        ServerTypeOffer {
            name: name.into(),
            description: String::new(),
            cores,
            memory_gb,
            disk_gb: 40,
            arch: "x86".into(),
            cpu_type: "shared".into(),
            prices: vec![LocationPrice {
                location: "hetzner:fsn1",
                monthly_cents: cents,
            }],
        }
    }

    #[test]
    fn picks_the_cheapest_type_that_fits() {
        let types = [
            offer("cx22", 2, 4.0, 452),
            offer("cx32", 4, 8.0, 822),
            offer("cpx11", 2, 2.0, 509),
            offer("cx42", 8, 16.0, 2000),
        ];
        let pick =
            |cpu, mem| cheapest_fit(&types, "hetzner:fsn1", cpu, mem).map(|(t, _)| t.name.as_str());
        assert_eq!(pick(500, 512), Some("cx22"));
        assert_eq!(pick(3000, 1024), Some("cx32"));
        // 4 GB minus the system reserve leaves 3.5 GB.
        assert_eq!(pick(500, 4000), Some("cx32"));
        assert_eq!(pick(500, 64_000), None);
        assert_eq!(
            cheapest_fit(&types, "hetzner:hel1", 1, 1).map(|(t, _)| t.name.clone()),
            None
        );
    }
}
