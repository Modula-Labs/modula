//! Step `dns` of a server migration: Cloudflare follows the server. Retried
//! with a backoff, each attempt idempotent, read back from Cloudflare, and
//! visible when it fails. A failure never undoes anything: the services run
//! on the new server and only this step is left (Retry in the dashboard).
//!
//! An attempt rewrites the moved services' custom domains, repoints every A
//! record in the workspace's zones exactly on the old address (Modula's or
//! not; opted-out ids stay) in place, and, for the main server, its names.
//! Without a DNS provider there is nothing to do and nothing to wait for.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use modula_id::Id;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::types::Json;
use time::OffsetDateTime;

use super::moves::{Table, begin, end};
use crate::audit::Entry;
use crate::deploy::env;
use crate::domains;
use crate::error::AppResult;
use crate::http::meta::RequestMeta;
use crate::integrations::cloudflare::{self, Connection, client::DnsRecord};
use crate::state::AppState;

pub const STEP_LABEL: &str = "Pointing DNS at the new server";
/// Attempts before the step is shown as failed.
pub const ATTEMPTS: i32 = 6;

fn backoff(attempt: i32) -> Duration {
    Duration::from_secs((5u64 << attempt.clamp(0, 5)).min(120))
}

/// A record changed by the step. `merged`: the same name already had the new
/// address, so the old record went instead of becoming a duplicate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub zone_id: String,
    pub zone: String,
    pub record_id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub from: String,
    pub to: String,
    pub action: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewRecord {
    pub id: String,
    pub zone: String,
    pub name: String,
    pub content: String,
    pub proxied: bool,
    /// Modula wrote it (the others are the user's own).
    pub managed: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DnsPreview {
    pub connected: bool,
    pub records: Vec<PreviewRecord>,
    /// Names also having AAAA records, which aren't changed.
    pub aaaa_names: Vec<String>,
    /// Cloudflare couldn't be read (the migration can still start).
    pub error: Option<String>,
}

struct Found {
    zone_id: String,
    zone: String,
    record: DnsRecord,
}

/// Every A record exactly on `ip` in the connection's active zones, and the
/// names among them that also have AAAA records.
async fn scan(c: &Connection, ip: &str) -> anyhow::Result<(Vec<Found>, BTreeSet<String>)> {
    let mut found = Vec::new();
    let mut aaaa = BTreeSet::new();
    for zone in c.client.all_zones().await? {
        if zone.status != "active" {
            continue;
        }
        let before = found.len();
        for record in c.client.all_records(&zone.id, "A").await? {
            if record.content == ip {
                found.push(Found {
                    zone_id: zone.id.clone(),
                    zone: zone.name.clone(),
                    record,
                });
            }
        }
        if found.len() > before {
            let names: BTreeSet<&str> = found[before..]
                .iter()
                .map(|f| f.record.name.as_str())
                .collect();
            for r in c.client.all_records(&zone.id, "AAAA").await? {
                if names.contains(r.name.as_str()) {
                    aaaa.insert(r.name);
                }
            }
        }
    }
    Ok((found, aaaa))
}

/// What the confirmation dialog shows. Never fails the preview.
pub async fn preview(state: &AppState, workspace: Option<Id>, node: Id) -> DnsPreview {
    let Some(workspace) = workspace else {
        return DnsPreview::default();
    };
    let ip: Option<String> = sqlx::query_scalar("SELECT public_ip FROM nodes WHERE id = $1")
        .bind(node)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    let conn = match cloudflare::connection(state, workspace).await {
        Ok(Some(c)) => c,
        Ok(None) => return DnsPreview::default(),
        Err(e) => {
            tracing::warn!(%workspace, error = ?e, "cannot load the Cloudflare integration");
            return DnsPreview {
                error: Some("the Cloudflare connection could not be loaded".into()),
                ..Default::default()
            };
        }
    };
    let Some(ip) = ip else {
        return DnsPreview {
            connected: true,
            ..Default::default()
        };
    };
    match scan(&conn, &ip).await {
        Ok((found, aaaa)) => DnsPreview {
            connected: true,
            records: found
                .into_iter()
                .map(|f| PreviewRecord {
                    id: f.record.id,
                    zone: f.zone,
                    name: f.record.name,
                    content: f.record.content,
                    proxied: f.record.proxied.unwrap_or(false),
                    managed: f.record.comment.as_deref()
                        == Some(cloudflare::client::MANAGED_COMMENT),
                })
                .collect(),
            aaaa_names: aaaa.into_iter().collect(),
            error: None,
        },
        Err(e) => DnsPreview {
            connected: true,
            error: Some(format!("{e:#}")),
            ..Default::default()
        },
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    workspace_id: Id,
    main: bool,
    source_ip: Option<String>,
    source_server_id: Option<Id>,
    target_ip: Option<String>,
    dns_skip: Json<Vec<String>>,
    dns_changes: Json<Vec<Change>>,
    dns_attempts: i32,
    dns_next_at: Option<OffsetDateTime>,
}

async fn load(state: &AppState, id: Id) -> AppResult<Option<Row>> {
    Ok(sqlx::query_as(
        "SELECT m.workspace_id, m.main, m.source_ip, m.source_server_id,
                coalesce(n.public_ip, t.ipv4) AS target_ip, m.dns_skip, m.dns_changes,
                m.dns_attempts, m.dns_next_at
         FROM server_migrations m
         LEFT JOIN provisioned_servers t ON t.id = m.target_server_id
         LEFT JOIN nodes n ON n.id = m.target_node_id
         WHERE m.id = $1 AND m.status = 'running' AND m.step = 'dns'",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?)
}

/// One poll of step `dns`; true once the migration is settled.
pub async fn poll(state: &AppState, id: Id) -> AppResult<bool> {
    let Some(r) = load(state, id).await? else {
        return Ok(true);
    };
    if r.dns_next_at.is_some_and(|t| t > OffsetDateTime::now_utc()) {
        return Ok(false);
    }
    let mut changes = r.dns_changes.0.clone();
    let result = attempt(state, id, &r, &mut changes).await;
    // Kept even after a failure: the next attempt continues from these.
    sqlx::query("UPDATE server_migrations SET dns_changes = $2 WHERE id = $1")
        .bind(id)
        .bind(Json(&changes))
        .execute(&state.db)
        .await?;
    match result {
        Ok(notes) => complete(state, id, &r, &changes, &notes).await,
        Err(e) => {
            let attempts = r.dns_attempts + 1;
            let why = format!("{e:#}");
            tracing::warn!(%id, attempts, error = %why, "the migration's DNS step failed");
            if attempts < ATTEMPTS {
                sqlx::query(
                    "UPDATE server_migrations SET dns_attempts = $2, dns_next_at = $3, updated_at = now()
                     WHERE id = $1 AND status = 'running'",
                )
                .bind(id)
                .bind(attempts)
                .bind(OffsetDateTime::now_utc() + backoff(attempts))
                .execute(&state.db)
                .await?;
                return Ok(false);
            }
            let error = format!(
                "the services run on the new server and their data moved; only DNS is left: {why}"
            );
            end(&state.db, Table::Migrations, id, "failed", Some(&why)).await?;
            sqlx::query(
                "UPDATE server_migrations SET status = 'failed', error = $2, dns_attempts = $3,
                        updated_at = now(), finished_at = now()
                 WHERE id = $1 AND status = 'running'",
            )
            .bind(id)
            .bind(&error)
            .bind(attempts)
            .execute(&state.db)
            .await?;
            Ok(true)
        }
    }
}

/// One idempotent attempt; `changes` accumulates across attempts. Returns the
/// notes for the step's detail.
async fn attempt(
    state: &AppState,
    id: Id,
    r: &Row,
    changes: &mut Vec<Change>,
) -> anyhow::Result<Vec<String>> {
    let moved: Vec<Id> = sqlx::query_scalar(
        "SELECT DISTINCT service_instance_id FROM instance_moves
         WHERE migration_id = $1 AND status = 'done'",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let conn = cloudflare::connection(state, r.workspace_id)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let mut notes = Vec::new();
    if conn.is_none() && !r.main {
        notes.push("no DNS provider connected: update your records yourself".to_owned());
        return Ok(notes);
    }
    let ip = r
        .target_ip
        .clone()
        .filter(|ip| ip.parse::<Ipv4Addr>().is_ok())
        .ok_or_else(|| anyhow::anyhow!("the new server has no public IPv4 address yet"))?;
    for i in &moved {
        domains::resync_instance(state, *i).await?;
    }
    if let (Some(c), Some(old)) = (&conn, r.source_ip.as_deref()) {
        repoint(c, old, &ip, &r.dns_skip.0, changes).await?;
    }
    if r.main {
        let hosts =
            super::promote::repoint_dns(state, r.source_ip.as_deref(), &ip, &r.dns_skip.0).await?;
        notes.extend(hosts.notes(&ip));
    }
    for i in &moved {
        domains::verify_instance(state, *i).await?;
    }
    if !changes.is_empty() {
        notes.insert(0, format!("{} DNS records repointed", changes.len()));
    }
    Ok(notes)
}

/// Point the A records on `old` at `new`, then check Cloudflare has it so.
async fn repoint(
    c: &Connection,
    old: &str,
    new: &str,
    skip: &[String],
    changes: &mut Vec<Change>,
) -> anyhow::Result<()> {
    if old == new {
        return Ok(());
    }
    let (found, _) = scan(c, old).await?;
    let mut zones: BTreeSet<String> = changes.iter().map(|ch| ch.zone_id.clone()).collect();
    for f in found.iter().filter(|f| !skip.contains(&f.record.id)) {
        zones.insert(f.zone_id.clone());
        // Two identical records are refused: drop the old one instead.
        let duplicate = c
            .client
            .dns_records(&f.zone_id, &f.record.name, "A")
            .await?
            .iter()
            .any(|r| r.content == new);
        let action = if duplicate {
            c.client.delete_record_id(&f.zone_id, &f.record.id).await?;
            "merged"
        } else {
            c.client
                .update_record_content(&f.zone_id, &f.record, new)
                .await?;
            "updated"
        };
        changes.retain(|ch| ch.record_id != f.record.id);
        changes.push(Change {
            zone_id: f.zone_id.clone(),
            zone: f.zone.clone(),
            record_id: f.record.id.clone(),
            name: f.record.name.clone(),
            kind: "A".into(),
            from: old.to_owned(),
            to: new.to_owned(),
            action: action.into(),
        });
    }
    for zone in zones {
        let now = c.client.all_records(&zone, "A").await?;
        if let Some(left) = now
            .iter()
            .find(|r| r.content == old && !skip.contains(&r.id))
        {
            anyhow::bail!("Cloudflare still has {} at {old}", left.name);
        }
        for ch in changes
            .iter()
            .filter(|ch| ch.zone_id == zone && ch.action == "updated")
        {
            if !now.iter().any(|r| r.id == ch.record_id && r.content == new) {
                anyhow::bail!("Cloudflare doesn't have {} at {new}", ch.name);
            }
        }
    }
    Ok(())
}

async fn complete(
    state: &AppState,
    id: Id,
    r: &Row,
    changes: &[Change],
    notes: &[String],
) -> AppResult<bool> {
    let detail = notes.join(" · ");
    let stale = match stale_tcp(state, id, r.source_ip.as_deref()).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(%id, error = ?e, "cannot list services holding an old TCP address");
            Vec::new()
        }
    };
    let mut tx = state.db.begin().await?;
    end(
        &mut *tx,
        Table::Migrations,
        id,
        "done",
        (!detail.is_empty()).then_some(detail.as_str()),
    )
    .await?;
    // A main server's was marked when the control plane moved.
    if !r.main {
        begin(
            &mut *tx,
            Table::Migrations,
            id,
            "Marking the old server for deletion",
        )
        .await?;
        if let Some(old) = r.source_server_id {
            sqlx::query(
                "UPDATE provisioned_servers SET status = 'pending_deletion', updated_at = now()
                 WHERE id = $1 AND status = 'ready'",
            )
            .bind(old)
            .execute(&mut *tx)
            .await?;
        }
        end(&mut *tx, Table::Migrations, id, "done", None).await?;
    }
    sqlx::query(
        "UPDATE server_migrations SET status = 'done', step = 'finish', stale_tcp = $2,
                updated_at = now(), finished_at = now()
         WHERE id = $1 AND status = 'running' AND step = 'dns'",
    )
    .bind(id)
    .bind(Json(&stale))
    .execute(&mut *tx)
    .await?;
    if !changes.is_empty() {
        let system = RequestMeta {
            ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            user_agent: Some("modula-server/migration".into()),
        };
        Entry::new("server.migrate.dns", &system)
            .workspace(r.workspace_id)
            .target("server_migration", id)
            .metadata(json!({ "records": changes }))
            .record(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// A service whose variables still hold a moved TCP proxy's old address.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StaleTcp {
    pub instance_id: Id,
    pub service: String,
    pub environment: String,
    pub project: String,
    /// The variables' names (never their values).
    pub variables: Vec<String>,
}

/// `value` contains `needle` not as part of a longer number or address.
fn holds(value: &str, needle: &str) -> bool {
    let edge = |c: Option<char>| c.is_none_or(|c| !c.is_ascii_digit() && c != '.');
    value.match_indices(needle).any(|(at, _)| {
        edge(value[..at].chars().next_back()) && edge(value[at + needle.len()..].chars().next())
    })
}

/// Variables are resolved when a service deploys and sealed in its deployment
/// (`deploy/start.rs`), so `${{db.MODULA_TCP_PROXY_DOMAIN}}` keeps the old
/// address in a service that isn't deployed again. Lists those.
async fn stale_tcp(state: &AppState, id: Id, source_ip: Option<&str>) -> AppResult<Vec<StaleTcp>> {
    let proxies: Vec<(Id, Option<i32>, i32)> = sqlx::query_as(
        "SELECT DISTINCT i.environment_id, mv.old_tcp_port, t.public_port
         FROM instance_moves mv
         JOIN service_instances i ON i.id = mv.service_instance_id
         JOIN tcp_proxies t ON t.service_instance_id = i.id
         WHERE mv.migration_id = $1 AND mv.status = 'done' AND mv.kind = 'primary'",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let mut out = Vec::new();
    let mut environments: Vec<Id> = proxies.iter().map(|p| p.0).collect();
    environments.sort();
    environments.dedup();
    for environment in environments {
        // Which text means the old address in this environment.
        let mut needles: Vec<String> = Vec::new();
        match (state.cfg.tcp_proxy_host.as_deref(), source_ip) {
            (None, Some(ip)) => needles.push(ip.to_owned()),
            (Some(host), _) => {
                for (_, old, new) in proxies.iter().filter(|p| p.0 == environment) {
                    if let Some(old) = old.filter(|o| o != new) {
                        needles.push(format!("{host}:{old}"));
                    }
                }
            }
            (None, None) => {}
        }
        if needles.is_empty() {
            continue;
        }
        let deployments: Vec<(Id, Id, Vec<u8>, String, String, String)> = sqlx::query_as(
            "SELECT d.id, i.id, d.env_sealed, s.name, e.name, p.name
             FROM deployments d
             JOIN service_instances i ON i.id = d.service_instance_id
             JOIN services s ON s.id = i.service_id
             JOIN environments e ON e.id = i.environment_id
             JOIN projects p ON p.id = s.project_id
             WHERE i.environment_id = $1 AND d.env_sealed IS NOT NULL
               AND d.status IN ('active', 'sleeping', 'crashed')
             ORDER BY s.name",
        )
        .bind(environment)
        .fetch_all(&state.db)
        .await?;
        for (deployment, instance, sealed, service, env_name, project) in deployments {
            let vars = env::open_snapshot(&state.keys, &state.db, environment, deployment, &sealed)
                .await?;
            let variables: Vec<String> = vars
                .iter()
                .filter(|(_, v)| needles.iter().any(|n| holds(v.expose_secret(), n)))
                .map(|(k, _)| k.clone())
                .collect();
            if !variables.is_empty() {
                out.push(StaleTcp {
                    instance_id: instance,
                    service,
                    environment: env_name,
                    project,
                    variables,
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_address_only() {
        assert!(holds("postgres://u:p@203.0.113.1:5432/x", "203.0.113.1"));
        assert!(holds("203.0.113.1", "203.0.113.1"));
        assert!(!holds("203.0.113.10:5432", "203.0.113.1"));
        assert!(!holds("1203.0.113.1", "203.0.113.1"));
        assert!(holds("db.example.com:20001/x", "db.example.com:20001"));
    }

    #[test]
    fn backoff_grows_and_stops() {
        assert_eq!(backoff(0), Duration::from_secs(5));
        assert_eq!(backoff(1), Duration::from_secs(10));
        assert_eq!(backoff(9), Duration::from_secs(120));
    }
}
