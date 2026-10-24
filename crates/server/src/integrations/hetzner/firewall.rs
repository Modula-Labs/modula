//! The private network through Hetzner firewalls: WireGuard (UDP 51820) is
//! open on Modula's firewalls only to Modula's own servers.
//!
//! Two kinds of Modula firewalls exist in a Hetzner project: the one per
//! workspace (and SSH choice) its provisioned workers share, and the main
//! server's (made by the `modula` CLI, `role=main`). Each carries one rule
//! described [`MESH_RULE`] whose sources are the public IPs of the servers
//! that peer with the ones behind it; [`SYNC_JOB`] rewrites that rule (and
//! only it) whenever a server joins or leaves the mesh.

use modula_id::Id;
use serde_json::json;

use super::client::{FirewallRule, MESH_RULE};
use crate::error::AppResult;
use crate::jobs::{self, Job, NewJob};
use crate::mesh::DEFAULT_WG_PORT;
use crate::state::AppState;

pub const SYNC_JOB: &str = "hetzner.sync_firewalls";

/// The WireGuard rule for `sources` (public IPv4s), or None when there are
/// none (Hetzner refuses an inbound rule without sources).
pub fn mesh_rule(sources: &[String]) -> Option<FirewallRule> {
    let mut cidrs: Vec<String> = sources.iter().map(|ip| format!("{ip}/32")).collect();
    cidrs.sort();
    cidrs.dedup();
    let port = DEFAULT_WG_PORT.to_string();
    (!cidrs.is_empty()).then(|| FirewallRule::inbound("udp", Some(&port), cidrs, MESH_RULE))
}

/// `rules` with their WireGuard rule replaced by the one for `sources`.
pub fn with_mesh_rule(rules: &[FirewallRule], sources: &[String]) -> Vec<FirewallRule> {
    let mut out: Vec<FirewallRule> = rules
        .iter()
        .filter(|r| r.description.as_deref() != Some(MESH_RULE))
        .cloned()
        .collect();
    out.extend(mesh_rule(sources));
    out
}

/// Public IPs of the servers in the private network that peer with
/// `workspace`'s (its own and shared ones), or with anyone (None: the main
/// server's firewall), including provisioned servers still joining.
pub async fn mesh_sources(db: &sqlx::PgPool, workspace: Option<Id>) -> AppResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT public_ip FROM nodes
         WHERE status <> 'revoked' AND wg_public_key IS NOT NULL AND public_ip IS NOT NULL
           AND ($1::text IS NULL OR workspace_id IS NULL OR workspace_id = $1)
         UNION
         SELECT ipv4 FROM provisioned_servers
         WHERE ipv4 IS NOT NULL AND status NOT IN ('failed', 'deleting')
           AND ($1::text IS NULL OR workspace_id = $1)
         ORDER BY 1",
    )
    .bind(workspace)
    .fetch_all(db)
    .await?)
}

/// Queues [`SYNC_JOB`] (once: queued syncs coalesce).
pub async fn enqueue_sync(db: impl sqlx::PgExecutor<'_>) -> AppResult<()> {
    jobs::enqueue(
        db,
        NewJob {
            kind: SYNC_JOB,
            payload: json!({}),
            max_attempts: Some(3),
            dedupe_key: Some(SYNC_JOB),
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}

/// Job: brings the WireGuard rule of every Modula firewall the connected
/// Hetzner projects hold up to date, leaving the other rules as they are.
pub async fn sync_job(state: AppState, _job: Job) -> anyhow::Result<()> {
    let workspaces: Vec<Id> =
        sqlx::query_scalar("SELECT workspace_id FROM integrations WHERE kind = 'hetzner'")
            .fetch_all(&state.db)
            .await?;
    let everyone = mesh_sources(&state.db, None).await?;
    let mut failed = Vec::new();
    for ws in workspaces {
        let Some(h) = super::provider(&state, ws).await? else {
            continue;
        };
        let own = mesh_sources(&state.db, Some(ws)).await?;
        let lists = [
            (format!("modula=1,workspace={ws}"), &own),
            ("modula=1,role=main".to_owned(), &everyone),
        ];
        for (selector, sources) in lists {
            let firewalls = match h.client().firewalls(&selector).await {
                Ok(f) => f,
                Err(e) => {
                    failed.push(format!("{ws}: {e}"));
                    continue;
                }
            };
            for fw in firewalls {
                let rules = with_mesh_rule(&fw.rules, sources);
                if rules == fw.rules {
                    continue;
                }
                if let Err(e) = h.client().set_firewall_rules(fw.id, &rules).await {
                    failed.push(format!("{}: {e}", fw.name));
                }
            }
        }
    }
    if !failed.is_empty() {
        anyhow::bail!("cannot update Hetzner firewalls: {}", failed.join("; "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_wireguard_rule_is_replaced() {
        let web = FirewallRule::inbound_any("tcp", Some("443"), "HTTPS");
        let old = mesh_rule(&["203.0.113.1".into()]).unwrap();
        let rules = with_mesh_rule(
            &[web.clone(), old],
            &["203.0.113.3".into(), "203.0.113.2".into()],
        );
        assert_eq!(rules[0], web);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[1].source_ips, ["203.0.113.2/32", "203.0.113.3/32"]);
        // No servers left: no rule (Hetzner refuses one without sources).
        assert_eq!(with_mesh_rule(&rules, &[]), [web]);
    }
}
