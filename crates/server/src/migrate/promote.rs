//! Migrating the main server: after the worker-style steps (the new server
//! joined, every service moved to it), the control plane itself moves. Step
//! `promote` of a `main` migration, in phases:
//!
//! - `bundle`: the dashboard goes read-only (super::maintenance), the new
//!   server's provider firewall opens the node port, and a bundle is built:
//!   a database dump (taken with this step already recorded, so the copy
//!   knows it is mid-migration), the master key, source uploads and the
//!   settings the installer needs. The new node gets `PromoteRequest` with a
//!   single-use ticket.
//! - `install`: the node fetches the bundle (`FetchPromotion`, its own client
//!   certificate + the ticket) and runs `install.sh main --restore`, which
//!   sets up Postgres, Redis and modula-server from it (the same CA: the
//!   `pki` row is sealed with the master key that came along). It reports
//!   with `PromoteStatus`; it stays connected here meanwhile.
//! - `verify`: its node endpoint must answer from here with a certificate
//!   from our CA (so the other servers can reach it).
//! - `switch`: the new node is told to dial its own control plane
//!   (`ServerAddress` to 127.0.0.1, then restart to serve the dashboard).
//!   Once it confirmed, this control plane retires: it records `retired_at`
//!   (only in its own database), tells every other node the new address,
//!   points the dashboard's DNS at the new server where Modula manages it,
//!   and from then on refuses everything, redirecting the dashboard and
//!   sending late nodes on.
//!
//! The copy on the new server starts with `MODULA_PROMOTED_MIGRATION` set and
//! the migration still at `promote` (maintenance on). When its own node
//! connects to it (which only happens once the old one gave the go) it
//! [`finalize`]s: the old server's node is revoked, so exactly one node
//! serves the dashboard, the old server is marked `pending_deletion`, and
//! maintenance ends.
//!
//! Before `switch` a failure ends maintenance here and leaves everything as
//! it was (abort moves the services back). From `switch` on there is no
//! automatic way back.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use modula_crypto::token;
use modula_id::Id;
use modula_proto::node::v1 as pb;
use rustls::pki_types::ServerName;
use time::OffsetDateTime;

use super::maintenance;
use super::moves::{Table, begin, end};
use crate::error::{AppError, AppResult};
use crate::integrations::cloudflare;
use crate::state::AppState;

/// How long the new server may take to download the bundle and install.
const INSTALL_WAIT: time::Duration = time::Duration::minutes(40);
/// A request the node never picked up (lost with a reconnect) is sent again.
const RESEND_AFTER: time::Duration = time::Duration::minutes(2);
/// How long its node endpoint may take to answer from here.
const VERIFY_WAIT: time::Duration = time::Duration::minutes(10);
/// How long the new node may take to confirm the switch.
const SWITCH_WAIT: time::Duration = time::Duration::minutes(3);
const CHUNK: usize = 1024 * 1024;

#[derive(sqlx::FromRow)]
struct Row {
    workspace_id: Id,
    source_node_id: Option<Id>,
    source_server_id: Option<Id>,
    target_server_id: Option<Id>,
    target_node_id: Option<Id>,
    target_name: Option<String>,
    target_provider_id: Option<String>,
    target_ip: Option<String>,
    status: String,
    step: String,
    promote_phase: Option<String>,
    promote_phase_at: Option<OffsetDateTime>,
    promote_fetched_at: Option<OffsetDateTime>,
    new_node_url: Option<String>,
}

async fn load(state: &AppState, id: Id) -> AppResult<Option<Row>> {
    Ok(sqlx::query_as(
        "SELECT m.workspace_id, m.source_node_id, m.source_server_id, m.target_server_id,
                m.target_node_id, t.name AS target_name, t.provider_id AS target_provider_id,
                coalesce(n.public_ip, t.ipv4) AS target_ip, m.status, m.step, m.promote_phase,
                m.promote_phase_at, m.promote_fetched_at, m.new_node_url
         FROM server_migrations m
         LEFT JOIN provisioned_servers t ON t.id = m.target_server_id
         LEFT JOIN nodes n ON n.id = m.target_node_id
         WHERE m.id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?)
}

fn is_ipv4(host: &str) -> bool {
    host.parse::<std::net::Ipv4Addr>().is_ok()
}

/// The control plane's addresses on the new server: the dashboard (env
/// `MODULA_PUBLIC_URL`: the same domain, or the new IP when it was reached
/// by IP), what users open (its dashboard domain when one is set), and the
/// node endpoint (always by IP: no DNS to wait for).
pub struct NewUrls {
    pub public_url: String,
    pub user_url: String,
    pub node_url: String,
}

pub fn new_urls(state: &AppState, new_ip: &str) -> NewUrls {
    let host = state.cfg.public_url.host_str().unwrap_or_default();
    let public_url = if is_ipv4(host) || host.is_empty() {
        format!("https://{new_ip}")
    } else {
        format!("https://{host}")
    };
    let user_url = match state.hosts().dashboard {
        Some(d) => format!("https://{d}"),
        None => public_url.clone(),
    };
    NewUrls {
        public_url,
        user_url,
        node_url: format!("https://{new_ip}:{}", state.cfg.node_listen.port()),
    }
}

/// The dashboard is reached by IP (its address changes with the server).
pub fn ip_based(state: &AppState) -> bool {
    state.hosts().dashboard.is_none() && state.cfg.public_url.host_str().is_none_or(is_ipv4)
}

/// Names whose DNS must point at the main server: the dashboard's and the
/// generated apps' wildcard.
pub fn dashboard_hosts(state: &AppState) -> Vec<String> {
    let mut hosts = Vec::new();
    if let Some(h) = state.cfg.public_url.host_str()
        && !is_ipv4(h)
        && h != "localhost"
    {
        hosts.push(h.to_owned());
    }
    let names = state.hosts();
    if let Some(d) = names.dashboard
        && !hosts.contains(&d)
    {
        hosts.push(d);
    }
    if let Some(a) = names.apps.filter(|a| a != "localhost") {
        hosts.push(format!("*.{a}"));
    }
    hosts
}

async fn set_phase(state: &AppState, id: Id, phase: &str) -> AppResult<()> {
    sqlx::query(
        "UPDATE server_migrations SET promote_phase = $2, promote_phase_at = now(), updated_at = now()
         WHERE id = $1 AND status = 'running'",
    )
    .bind(id)
    .bind(phase)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Every service moved: put the dashboard in maintenance and start copying
/// the control plane.
pub async fn enter(state: &AppState, id: Id) -> AppResult<bool> {
    let Some(r) = load(state, id).await? else {
        return Ok(true);
    };
    let Some(ip) = r.target_ip.clone().filter(|ip| is_ipv4(ip)) else {
        return super::server::fail(state, id, "the new server has no public IPv4 address").await;
    };
    let urls = new_urls(state, &ip);
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "UPDATE server_migrations SET step = 'promote', promote_phase = 'bundle', promote_phase_at = now(),
                new_public_url = $2, new_node_url = $3, promote_ticket_hash = NULL,
                promote_fetched_at = NULL, promote_detail = NULL, updated_at = now()
         WHERE id = $1 AND status = 'running'",
    )
    .bind(id)
    .bind(&urls.user_url)
    .bind(&urls.node_url)
    .execute(&mut *tx)
    .await?;
    begin(
        &mut *tx,
        Table::Migrations,
        id,
        "Pausing changes and copying the control plane",
    )
    .await?;
    tx.commit().await?;
    maintenance::sync(state).await?;

    // The other servers must reach its node endpoint.
    if let (Some(pid), Some(name)) = (r.target_provider_id.as_deref(), r.target_name.as_deref()) {
        let kind: Option<String> =
            sqlx::query_scalar("SELECT provider FROM provisioned_servers WHERE id = $1")
                .bind(r.target_server_id)
                .fetch_optional(&state.db)
                .await?;
        let provider = match kind {
            Some(k) => crate::provision::resize::provider_for(state, r.workspace_id, &k).await?,
            None => None,
        };
        let Some(provider) = provider else {
            return super::server::fail(
                state,
                id,
                "the cloud provider of the new server is no longer connected",
            )
            .await;
        };
        if let Err(e) = provider
            .open_node_port(pid, name, state.cfg.node_listen.port())
            .await
        {
            let why = format!("couldn't open the node port in the new server's firewall: {e}");
            return super::server::fail(state, id, &why).await;
        }
    }
    Ok(false)
}

/// One poll of step `promote` (the old control plane's side, or the new
/// one's when it was restored from this migration).
pub async fn poll(state: &AppState, id: Id) -> AppResult<bool> {
    if state.cfg.promoted_migration == Some(id) {
        return poll_new(state, id).await;
    }
    let Some(r) = load(state, id).await? else {
        return Ok(true);
    };
    if r.status != "running" || r.step != "promote" {
        return Ok(true);
    }
    let Some(target) = r.target_node_id else {
        return super::server::fail(state, id, "the new server was removed").await;
    };
    let since = r
        .promote_phase_at
        .map_or(time::Duration::ZERO, |t| OffsetDateTime::now_utc() - t);
    match r.promote_phase.as_deref().unwrap_or("bundle") {
        "bundle" => bundle_phase(state, id, &r, target).await,
        "install" => {
            if since > INSTALL_WAIT {
                return super::server::fail(
                    state,
                    id,
                    "the new server did not finish installing the control plane within 40 minutes",
                )
                .await;
            }
            if r.promote_fetched_at.is_none()
                && since > RESEND_AFTER
                && state.hub.is_connected(target)
            {
                send_request(state, id, target).await?;
            }
            Ok(false)
        }
        "verify" => {
            let node_url = r.new_node_url.clone().unwrap_or_default();
            match verify(state, &node_url).await {
                Ok(()) => {
                    set_phase(state, id, "switch").await?;
                    end(&state.db, Table::Migrations, id, "done", None).await?;
                    begin(
                        &state.db,
                        Table::Migrations,
                        id,
                        "Handing over to the new server",
                    )
                    .await?;
                    let port = state.cfg.node_listen.port();
                    let sent = state
                        .hub
                        .send(
                            target,
                            pb::server_message::Msg::ServerAddress(pb::ServerAddress {
                                request_id: id.to_string(),
                                server: format!("https://127.0.0.1:{port}"),
                                restart: true,
                            }),
                        )
                        .await;
                    if !sent {
                        return super::server::fail(
                            state,
                            id,
                            "the new server disconnected before the hand-over",
                        )
                        .await;
                    }
                    Ok(false)
                }
                Err(e) if since > VERIFY_WAIT => {
                    let why = format!(
                        "the new control plane doesn't answer at {node_url} ({e:#}); \
                         is port {} open in front of the new server?",
                        state.cfg.node_listen.port()
                    );
                    super::server::fail(state, id, &why).await
                }
                Err(e) => {
                    tracing::info!(error = format!("{e:#}"), %node_url, "new control plane not reachable yet");
                    Ok(false)
                }
            }
        }
        "switch" => match take_address_status(target, id) {
            Some(Ok(())) => commit(state, id, &r).await,
            Some(Err(e)) => {
                let why = format!("the new server couldn't reach its own control plane: {e}");
                super::server::fail(state, id, &why).await
            }
            // It answers before switching, so silence plus a dropped
            // connection means it moved and the answer was lost.
            None if since > SWITCH_WAIT && !state.hub.is_connected(target) => {
                commit(state, id, &r).await
            }
            None if since > SWITCH_WAIT => {
                super::server::fail(
                    state,
                    id,
                    "the new server did not confirm the hand-over within 3 minutes",
                )
                .await
            }
            None => Ok(false),
        },
        _ => Ok(true),
    }
}

async fn bundle_phase(state: &AppState, id: Id, r: &Row, target: Id) -> AppResult<bool> {
    // Anything placed on the old server before maintenance began.
    if let Some(source) = r.source_node_id
        && super::server::plan_moves(state, id, source, target).await? > 0
    {
        super::server::set_step(state, id, "move").await?;
        begin(&state.db, Table::Migrations, id, "Moving the last services").await?;
        return Ok(false);
    }
    let path = bundle_path(state, id);
    if path.exists() {
        end(&state.db, Table::Migrations, id, "done", None).await?;
        begin(
            &state.db,
            Table::Migrations,
            id,
            "Installing the control plane on the new server",
        )
        .await?;
        set_phase(state, id, "install").await?;
        if !state.hub.is_connected(target) {
            // Sent again from `install` once it is back.
            return Ok(false);
        }
        send_request(state, id, target).await?;
        return Ok(false);
    }
    if let Some(e) = take_build_error(id) {
        return super::server::fail(state, id, &format!("couldn't copy the control plane: {e}"))
            .await;
    }
    if building().lock().expect("builds").insert(id) {
        let st = state.clone();
        tokio::spawn(async move {
            let result = build_bundle(&st, id).await;
            if let Err(e) = &result {
                tracing::warn!(%id, error = format!("{e:#}"), "cannot build the promotion bundle");
                build_errors()
                    .lock()
                    .expect("builds")
                    .insert(id, format!("{e:#}"));
            }
            building().lock().expect("builds").remove(&id);
        });
    }
    Ok(false)
}

/// A new single-use ticket, then the request.
async fn send_request(state: &AppState, id: Id, target: Id) -> AppResult<()> {
    let ticket = token::generate(token::prefix::PROMOTION)?;
    sqlx::query(
        "UPDATE server_migrations SET promote_ticket_hash = $2, promote_fetched_at = NULL,
                promote_phase_at = now(), updated_at = now()
         WHERE id = $1 AND status = 'running'",
    )
    .bind(id)
    .bind(ticket.hash.as_bytes())
    .execute(&state.db)
    .await?;
    use secrecy::ExposeSecret;
    let msg = pb::server_message::Msg::Promote(pb::PromoteRequest {
        request_id: id.to_string(),
        ticket: ticket.plaintext.expose_secret().to_owned(),
    });
    if !state.hub.send(target, msg).await {
        tracing::info!(%id, "the new server is offline; asking again later");
    }
    Ok(())
}

// ---------------------------------------------------------------- the bundle

fn building() -> &'static Mutex<HashSet<Id>> {
    static B: OnceLock<Mutex<HashSet<Id>>> = OnceLock::new();
    B.get_or_init(Default::default)
}

fn build_errors() -> &'static Mutex<HashMap<Id, String>> {
    static E: OnceLock<Mutex<HashMap<Id, String>>> = OnceLock::new();
    E.get_or_init(Default::default)
}

fn take_build_error(id: Id) -> Option<String> {
    build_errors().lock().expect("builds").remove(&id)
}

/// Next to the master key (the service's state directory).
fn bundle_dir(state: &AppState) -> PathBuf {
    match state.cfg.master_key_file.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join("promotion"),
        _ => std::env::temp_dir().join("modula-promotion"),
    }
}

pub fn bundle_path(state: &AppState, id: Id) -> PathBuf {
    bundle_dir(state).join(format!("{id}.tar"))
}

/// Remove a migration's bundle (it holds the master key).
pub fn remove_bundle(state: &AppState, id: Id) {
    let _ = std::fs::remove_file(bundle_path(state, id));
}

/// `promotion.env` for install.sh (KEY=VALUE, read without sourcing).
fn settings(state: &AppState, id: Id, new_ip: &str) -> String {
    let urls = new_urls(state, new_ip);
    let cfg = &state.cfg;
    let mut out = format!(
        "MIGRATION_ID={id}\nPUBLIC_URL={}\nNODE_URL={}\nNODE_PORT={}\nVERSION={}\n",
        urls.public_url,
        urls.node_url,
        cfg.node_listen.port(),
        env!("CARGO_PKG_VERSION"),
    );
    if let Some(e) = &cfg.acme_email {
        out.push_str(&format!("ACME_EMAIL={e}\n"));
    }
    if let Some(a) = cfg.apps_domain.as_ref().filter(|a| *a != "localhost") {
        out.push_str(&format!("APPS_DOMAIN={a}\n"));
    }
    // An address of the old server would be wrong there.
    if let Some(h) = cfg.tcp_proxy_host.as_ref().filter(|h| !is_ipv4(h)) {
        out.push_str(&format!("TCP_PROXY_HOST={h}\n"));
    }
    out
}

async fn build_bundle(state: &AppState, id: Id) -> anyhow::Result<()> {
    let ip: String = sqlx::query_scalar(
        "SELECT coalesce(n.public_ip, t.ipv4) FROM server_migrations m
         LEFT JOIN provisioned_servers t ON t.id = m.target_server_id
         LEFT JOIN nodes n ON n.id = m.target_node_id WHERE m.id = $1",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    let dir = bundle_dir(state);
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let dump = dir.join(format!("{id}.dump"));
    let mut cmd = crate::backup::control_plane::pg_dump(&state.cfg.database_url)?;
    let out = cmd
        .arg(format!("--file={}", dump.display()))
        .stdout(Stdio::null())
        .output()
        .await
        .map_err(|e| {
            anyhow::anyhow!("cannot run pg_dump (is the PostgreSQL client installed?): {e}")
        })?;
    if !out.status.success() {
        anyhow::bail!(
            "pg_dump failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let env = settings(state, id, &ip);
    let master = state.cfg.master_key_file.clone();
    let uploads = state.cfg.uploads_dir.clone();
    let path = bundle_path(state, id);
    let part = path.with_extension("tar.part");
    let dump_for_tar = dump.clone();
    let written = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        write_tar(&part, &dump_for_tar, &master, &uploads, &env)?;
        std::fs::rename(&part, &path)?;
        Ok(())
    })
    .await?;
    let _ = std::fs::remove_file(&dump);
    written?;
    tracing::info!(%id, "promotion bundle ready");
    Ok(())
}

fn write_tar(
    to: &Path,
    dump: &Path,
    master: &Path,
    uploads: &Path,
    env: &str,
) -> anyhow::Result<()> {
    use anyhow::Context;
    let file = {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        o.open(to)?
    };
    let mut tar = tar::Builder::new(file);
    tar.mode(tar::HeaderMode::Deterministic);
    let mut header = tar::Header::new_gnu();
    header.set_size(env.len() as u64);
    header.set_mode(0o600);
    header.set_cksum();
    tar.append_data(&mut header, "promotion.env", env.as_bytes())?;
    tar.append_path_with_name(dump, "control-plane.dump")?;
    tar.append_path_with_name(master, "master.key")
        .with_context(|| format!("cannot read the master key {}", master.display()))?;
    if uploads.is_dir() {
        tar.follow_symlinks(false);
        append_regular(&mut tar, uploads, Path::new("uploads"))?;
    }
    let file = tar.into_inner()?;
    file.sync_all()?;
    Ok(())
}

/// Directories and regular files only (the installer refuses anything else).
fn append_regular(
    tar: &mut tar::Builder<std::fs::File>,
    dir: &Path,
    name: &Path,
) -> anyhow::Result<()> {
    tar.append_dir(name, dir)?;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let child = name.join(entry.file_name());
        if kind.is_dir() {
            append_regular(tar, &entry.path(), &child)?;
        } else if kind.is_file() {
            tar.append_path_with_name(entry.path(), &child)?;
        }
    }
    Ok(())
}

/// `FetchPromotion`: the bundle for `node`, once per ticket.
pub async fn open_bundle(
    state: &AppState,
    node: Id,
    req: &pb::PromotionFetch,
) -> AppResult<PathBuf> {
    let id: Id = req.request_id.parse().map_err(|_| AppError::NotFound)?;
    if !token::is_well_formed(&req.ticket, token::prefix::PROMOTION) {
        return Err(AppError::Forbidden);
    }
    let hash = token::hash(&req.ticket);
    let claimed: Option<Id> = sqlx::query_scalar(
        "UPDATE server_migrations SET promote_fetched_at = now(), updated_at = now()
         WHERE id = $1 AND main AND status = 'running' AND step = 'promote' AND promote_phase = 'install'
           AND target_node_id = $2 AND promote_ticket_hash = $3 AND promote_fetched_at IS NULL
         RETURNING id",
    )
    .bind(id)
    .bind(node)
    .bind(hash.as_bytes())
    .fetch_optional(&state.db)
    .await?;
    if claimed.is_none() {
        return Err(AppError::Forbidden);
    }
    let path = bundle_path(state, id);
    if !path.is_file() {
        return Err(AppError::NotFound);
    }
    Ok(path)
}

/// Stream `path` in chunks, the last one carrying size and SHA-256.
pub fn stream_file(
    path: PathBuf,
) -> tokio_stream::wrappers::ReceiverStream<Result<pb::PromotionChunk, tonic::Status>> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt;
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        let mut file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => {
                let _ = tx.send(Err(tonic::Status::internal(e.to_string()))).await;
                return;
            }
        };
        let mut hash = Sha256::new();
        let mut total = 0u64;
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = match file.read(&mut buf).await {
                Ok(n) => n,
                Err(e) => {
                    let _ = tx.send(Err(tonic::Status::internal(e.to_string()))).await;
                    return;
                }
            };
            if n == 0 {
                break;
            }
            hash.update(&buf[..n]);
            total += n as u64;
            let chunk = pb::PromotionChunk {
                data: buf[..n].to_vec(),
                ..Default::default()
            };
            if tx.send(Ok(chunk)).await.is_err() {
                return;
            }
        }
        let last = pb::PromotionChunk {
            data: Vec::new(),
            sha256: hex::encode(hash.finalize()),
            total_bytes: total,
        };
        let _ = tx.send(Ok(last)).await;
    });
    tokio_stream::wrappers::ReceiverStream::new(rx)
}

// ---------------------------------------------------------------- node answers

/// `PromoteStatus` from the new server.
pub async fn on_status(state: &AppState, node: Id, s: pb::PromoteStatus) -> anyhow::Result<()> {
    let Ok(id) = s.request_id.parse::<Id>() else {
        return Ok(());
    };
    let ours: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM server_migrations WHERE id = $1 AND target_node_id = $2
                        AND status = 'running' AND step = 'promote' AND promote_phase = 'install')",
    )
    .bind(id)
    .bind(node)
    .fetch_one(&state.db)
    .await?;
    if !ours {
        return Ok(());
    }
    if !s.done {
        let step: String = s.step.chars().take(200).collect();
        sqlx::query(
            "UPDATE server_migrations SET promote_detail = $2, updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(&step)
        .execute(&state.db)
        .await?;
        return Ok(());
    }
    if s.ok {
        end(&state.db, Table::Migrations, id, "done", None).await?;
        begin(
            &state.db,
            Table::Migrations,
            id,
            "Checking the new control plane",
        )
        .await?;
        set_phase(state, id, "verify").await?;
        remove_bundle(state, id);
        super::server::enqueue(state, id, Duration::ZERO)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    } else {
        let error: String = s.error.chars().take(2000).collect();
        super::server::fail(
            state,
            id,
            &format!("installing on the new server failed: {error}"),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    Ok(())
}

type AddressAnswers = Mutex<HashMap<(Id, String), Result<(), String>>>;

fn answers() -> &'static AddressAnswers {
    static A: OnceLock<AddressAnswers> = OnceLock::new();
    A.get_or_init(Default::default)
}

/// `ServerAddressStatus` from any node.
pub fn on_address_status(node: Id, s: pb::ServerAddressStatus) {
    let result = if s.ok {
        Ok(())
    } else {
        Err(s.error.chars().take(1000).collect())
    };
    if let Err(e) = &result {
        tracing::warn!(%node, error = %e, "a server couldn't switch to the new control plane");
    }
    answers()
        .lock()
        .expect("answers")
        .insert((node, s.request_id), result);
}

fn take_address_status(node: Id, id: Id) -> Option<Result<(), String>> {
    answers()
        .lock()
        .expect("answers")
        .remove(&(node, id.to_string()))
}

// ---------------------------------------------------------------- verify

/// A TLS handshake with `node_url` that only a server holding a certificate
/// from this instance's CA (with that address in it) can complete.
pub async fn verify(state: &AppState, node_url: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    let url: url::Url = node_url.parse().context("invalid node URL")?;
    let host = url.host_str().context("no host")?.to_owned();
    let port = url.port().unwrap_or(443);
    let pki = state.pki().await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(rustls::pki_types::CertificateDer::from(
        pki.root_der().to_vec(),
    ))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let name = ServerName::try_from(host.clone())?;
    let tcp = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await
    .context("timed out")??;
    tokio::time::timeout(Duration::from_secs(5), connector.connect(name, tcp))
        .await
        .context("TLS timed out")??;
    Ok(())
}

// ---------------------------------------------------------------- hand-over

/// The new node switched: retire this control plane and send everyone on.
async fn commit(state: &AppState, id: Id, r: &Row) -> AppResult<bool> {
    let node_url = r.new_node_url.clone().unwrap_or_default();
    let mut tx = state.db.begin().await?;
    let done = sqlx::query(
        "UPDATE server_migrations SET retired_at = now(), status = 'done', step = 'finish',
                updated_at = now(), finished_at = now()
         WHERE id = $1 AND status = 'running' AND step = 'promote'",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        return Ok(true);
    }
    if let Some(old) = r.source_server_id {
        sqlx::query(
            "UPDATE provisioned_servers SET status = 'pending_deletion', updated_at = now()
             WHERE id = $1 AND status = 'ready'",
        )
        .bind(old)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    maintenance::sync(state).await?;
    tracing::info!(%id, to = %node_url, "the control plane moved; this one retires");

    let mut notes = Vec::new();
    // Every other node (late ones are sent on when they connect).
    let nodes: Vec<(Id, String, bool)> = sqlx::query_as(
        "SELECT id, name, coalesce(info->'capabilities' ? 'server-address', false) FROM nodes
         WHERE status <> 'revoked' AND id <> $1 AND id IS DISTINCT FROM $2",
    )
    .bind(r.target_node_id)
    .bind(r.source_node_id)
    .fetch_all(&state.db)
    .await?;
    let mut old_nodes = Vec::new();
    for (node, name, capable) in nodes {
        if !capable {
            old_nodes.push(name);
            continue;
        }
        send_address(state, node, id, &node_url).await;
    }
    if !old_nodes.is_empty() {
        notes.push(format!(
            "{} run an older modula-node that can't be told the new address: \
             re-join them, or upgrade them first and start over",
            old_nodes.join(", ")
        ));
    }
    let ip = r.target_ip.clone().unwrap_or_default();
    notes.extend(repoint_dns(state, &ip).await);
    let detail = notes.join(" · ");
    end(
        &state.db,
        Table::Migrations,
        id,
        "done",
        (!detail.is_empty()).then_some(detail.as_str()),
    )
    .await?;
    Ok(true)
}

/// Tell `node` to dial `node_url` from now on.
pub async fn send_address(state: &AppState, node: Id, id: Id, node_url: &str) -> bool {
    state
        .hub
        .send(
            node,
            pb::server_message::Msg::ServerAddress(pb::ServerAddress {
                request_id: id.to_string(),
                server: node_url.to_owned(),
                restart: false,
            }),
        )
        .await
}

/// Point the dashboard's (and generated apps') DNS at `ip` where Modula wrote
/// the records in a connected Cloudflare zone; say what to change elsewhere.
async fn repoint_dns(state: &AppState, ip: &str) -> Vec<String> {
    let hosts = dashboard_hosts(state);
    if hosts.is_empty() || !is_ipv4(ip) {
        return Vec::new();
    }
    let workspaces: Vec<Id> = sqlx::query_scalar(
        "SELECT workspace_id FROM integrations WHERE kind = 'cloudflare' ORDER BY created_at",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    let mut connections = Vec::new();
    for ws in workspaces {
        if let Ok(Some(c)) = cloudflare::connection(state, ws).await {
            connections.push(c);
        }
    }
    let mut pointed = Vec::new();
    let mut manual = Vec::new();
    for host in hosts {
        let mut done = false;
        for c in &connections {
            let Ok(Some(zone)) = c.client.find_zone(&host).await else {
                continue;
            };
            let managed: Vec<cloudflare::client::DnsRecord> = c
                .client
                .dns_records(&zone.id, &host, "A")
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|r| r.comment.as_deref() == Some(cloudflare::client::MANAGED_COMMENT))
                .collect();
            let Some(first) = managed.first() else {
                continue;
            };
            let proxied = first.proxied.unwrap_or(false);
            match c
                .client
                .set_routing(&zone.id, &host, &[("A", ip)], proxied)
                .await
            {
                Ok(()) => {
                    pointed.push(host.clone());
                    done = true;
                }
                Err(e) => tracing::warn!(%host, error = %e, "cannot repoint DNS"),
            }
            break;
        }
        if !done {
            manual.push(host);
        }
    }
    let mut notes = Vec::new();
    if !pointed.is_empty() {
        notes.push(format!("DNS: pointed {} at {ip}", pointed.join(", ")));
    }
    if !manual.is_empty() {
        notes.push(format!(
            "Change the DNS A record of {} to {ip} yourself",
            manual.join(", ")
        ));
    }
    notes
}

// ---------------------------------------------------------------- the new control plane

/// Startup: maintenance as recorded, and (on the new server) keep driving
/// the migration this copy was restored from.
pub async fn startup(state: &AppState) -> AppResult<()> {
    maintenance::sync(state).await?;
    if let Some(id) = state.cfg.promoted_migration {
        let pending: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM server_migrations WHERE id = $1 AND status = 'running'
                            AND step = 'promote')",
        )
        .bind(id)
        .fetch_one(&state.db)
        .await?;
        if pending {
            tracing::info!(%id, "restored by a main server migration; waiting for the hand-over");
            super::server::enqueue(state, id, Duration::ZERO).await?;
        }
    }
    Ok(())
}

/// The new control plane: finish once its own node dials it (the old one
/// told it to, which is the go).
async fn poll_new(state: &AppState, id: Id) -> AppResult<bool> {
    let Some(r) = load(state, id).await? else {
        return Ok(true);
    };
    if r.status != "running" || r.step != "promote" {
        return Ok(true);
    }
    match r.target_node_id {
        Some(target) if state.hub.is_connected(target) => {
            finalize(state, id).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// This control plane is the main one now: the old server's node goes, its
/// server is marked for deletion, maintenance ends.
pub async fn finalize(state: &AppState, id: Id) -> AppResult<()> {
    let Some(r) = load(state, id).await? else {
        return Ok(());
    };
    let mut tx = state.db.begin().await?;
    let done = sqlx::query(
        "UPDATE server_migrations SET status = 'done', step = 'finish', updated_at = now(),
                finished_at = now(), promote_ticket_hash = NULL
         WHERE id = $1 AND status = 'running' AND step = 'promote'",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        return Ok(());
    }
    end(&mut *tx, Table::Migrations, id, "done", None).await?;
    begin(
        &mut *tx,
        Table::Migrations,
        id,
        "The new server runs the dashboard",
    )
    .await?;
    end(&mut *tx, Table::Migrations, id, "done", None).await?;
    if let Some(old) = r.source_node_id {
        crate::nodes::revoke(&mut tx, old).await?;
    }
    if let Some(old) = r.source_server_id {
        sqlx::query(
            "UPDATE provisioned_servers SET status = 'pending_deletion', updated_at = now()
             WHERE id = $1 AND status IN ('ready', 'resizing')",
        )
        .bind(old)
        .execute(&mut *tx)
        .await?;
    }
    if let Some(new) = r.target_node_id {
        crate::domains::resync_for_node(&mut *tx, new, None).await?;
    }
    tx.commit().await?;
    if let Some(old) = r.source_node_id {
        state.hub.disconnect(old);
    }
    maintenance::sync(state).await?;
    state.jobs_notify.notify_one();
    tracing::info!(%id, "main server migration finished; this is the main server now");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_after_the_move() {
        let state_less = |public: &str| {
            let cfg = crate::config::Config::for_tests(public);
            let host = cfg.public_url.host_str().unwrap_or_default().to_owned();
            is_ipv4(&host)
        };
        assert!(state_less("https://203.0.113.5"));
        assert!(!state_less("https://deploy.example.com"));
    }
}
