//! Publishing a static site's build output to Cloudflare as Workers static
//! assets (a job, once the node that built the deployment has uploaded the
//! output; see `deploy::output` and `deploy::state`).
//!
//! Why Workers static assets rather than Pages: Cloudflare recommends Workers
//! for new projects; one script upload sets the assets, the SPA fallback
//! (`not_found_handling`) and every variable and secret atomically, so a
//! deployment is exactly what Modula has (Modula is the source of truth:
//! bindings not in Modula are removed); and custom domains attach to a Worker
//! with one call that also creates the DNS record and certificate.
//!
//! Why the server uploads rather than the node: the Cloudflare token stays
//! sealed on the server and never reaches a node; the node only holds a
//! deployment-bound token for handing the output to the server.
//!
//! Every step writes a deployment event, so the deployment's timeline shows
//! the publish like a rollout; a Cloudflare error fails the deployment with
//! a message saying what to fix.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path};

use modula_id::Id;
use secrecy::ExposeSecret;

use super::client;
use super::workers::{self, AssetFile, Binding, ManifestEntry};
use crate::deploy::{config::ServiceConfig, env, output};
use crate::error::AppResult;
use crate::jobs::Job;
use crate::state::AppState;

pub const JOB: &str = "cloudflare.publish";

/// Cloudflare's per-file limit for Workers static assets.
pub const MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;
/// Cloudflare's per-version file limit on paid plans (free: 20,000).
pub const MAX_FILES: usize = 100_000;

/// Variables only a build reads (frameworks inline them into the bundle);
/// they are not set on the Worker.
const BUILD_ONLY_PREFIXES: &[&str] = &[
    "VITE_",
    "NEXT_PUBLIC_",
    "PUBLIC_",
    "REACT_APP_",
    "NUXT_PUBLIC_",
    "GATSBY_",
    "EXPO_PUBLIC_",
    "RAILPACK_",
    "NIXPACKS_",
];

#[derive(sqlx::FromRow)]
struct Row {
    status: String,
    service_instance_id: Id,
    config: serde_json::Value,
    env_sealed: Option<Vec<u8>>,
    vars_sealed: Option<Vec<u8>>,
    environment_id: Id,
    environment_name: String,
    service_name: String,
    workspace_id: Id,
    reports_commit: bool,
}

/// A failure shown to the user as the deployment's error.
struct Failed(String);

impl From<client::Error> for Failed {
    fn from(e: client::Error) -> Self {
        if e.is_forbidden() {
            Failed(format!(
                "{e}. The Cloudflare token needs Account · Workers Scripts · Edit: create a new token from the template in Settings → Integrations → Cloudflare"
            ))
        } else {
            Failed(e.to_string())
        }
    }
}

impl From<crate::error::AppError> for Failed {
    fn from(e: crate::error::AppError) -> Self {
        Failed(e.to_string())
    }
}

/// Before a deployment hosted on Cloudflare starts building: the workspace
/// must have a Cloudflare connection that sees an account.
pub async fn preflight(state: &AppState, instance: Id) -> AppResult<()> {
    let meta: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT g.metadata FROM service_instances i
         JOIN services s ON s.id = i.service_id
         JOIN projects p ON p.id = s.project_id
         JOIN integrations g ON g.workspace_id = p.workspace_id AND g.kind = 'cloudflare'
         WHERE i.id = $1",
    )
    .bind(instance)
    .fetch_optional(&state.db)
    .await?;
    let Some(meta) = meta else {
        return Err(crate::error::AppError::Conflict(
            "this service is hosted on Cloudflare, but Cloudflare isn't connected: connect it in Settings → Integrations, or switch hosting to your servers".into(),
        ));
    };
    let meta: super::Metadata = serde_json::from_value(meta).unwrap_or_default();
    if meta.account_id.is_none() {
        return Err(crate::error::AppError::Conflict(
            "the Cloudflare token can't see an account; reconnect it with one to host on Cloudflare".into(),
        ));
    }
    Ok(())
}

/// The job: `{deploymentId, sha256}`.
pub async fn job(state: AppState, job: Job) -> anyhow::Result<()> {
    let dep: Id = job
        .payload
        .get("deploymentId")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("bad payload"))?;
    let sha256 = job
        .payload
        .get("sha256")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    publish(&state, dep, &sha256).await?;
    Ok(())
}

/// Publishes one deployment if it is still waiting to be. Cloudflare
/// failures fail the deployment; only database errors are returned (the job
/// then retries).
pub async fn publish(state: &AppState, dep: Id, sha256: &str) -> AppResult<()> {
    let row: Option<Row> = sqlx::query_as(
        "SELECT d.status, d.service_instance_id, d.config, d.env_sealed, d.vars_sealed,
                i.environment_id, e.name AS environment_name, s.name AS service_name,
                p.workspace_id,
                d.commit_sha IS NOT NULL AND d.trigger IN ('push', 'manual')
                  AND EXISTS (SELECT 1 FROM github_apps) AS reports_commit
         FROM deployments d
         JOIN service_instances i ON i.id = d.service_instance_id
         JOIN environments e ON e.id = i.environment_id
         JOIN services s ON s.id = i.service_id
         JOIN projects p ON p.id = s.project_id
         WHERE d.id = $1",
    )
    .bind(dep)
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else {
        output::discard(state, dep).await;
        return Ok(());
    };
    if row.status != "deploying" {
        output::discard(state, dep).await;
        return Ok(());
    }
    let scratch = output::dir(state).join(dep.to_string());
    let result = run(state, dep, sha256, &row, &scratch).await;
    let _ = tokio::fs::remove_dir_all(&scratch).await;
    output::discard(state, dep).await;
    match result {
        Ok(url) => finish(state, dep, &row, Ok(url)).await,
        Err(Failed(msg)) => finish(state, dep, &row, Err(msg)).await,
    }
}

async fn event(state: &AppState, dep: Id, phase: &str, message: &str) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO deployment_events (deployment_id, phase, message) VALUES ($1, $2, $3)",
    )
    .bind(dep)
    .bind(phase)
    .bind(message)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// The Worker's name: configured, else `<service>-<environment>`.
pub fn script_name(configured: Option<&str>, service: &str, environment: &str) -> String {
    if let Some(n) = configured {
        return n.to_owned();
    }
    let raw = format!("{service}-{environment}").to_ascii_lowercase();
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        let c = if c.is_ascii_alphanumeric() { c } else { '-' };
        if !(c == '-' && out.ends_with('-')) {
            out.push(c);
        }
    }
    let out: String = out.trim_matches('-').chars().take(63).collect();
    let out = out.trim_end_matches('-').to_owned();
    if out.is_empty() { "site".into() } else { out }
}

async fn run(
    state: &AppState,
    dep: Id,
    sha256: &str,
    row: &Row,
    scratch: &Path,
) -> Result<Option<String>, Failed> {
    let cfg: ServiceConfig = serde_json::from_value(row.config.clone())
        .map_err(|e| Failed(format!("invalid config snapshot: {e}")))?;
    let cf = cfg
        .cloudflare()
        .ok_or_else(|| Failed("this deployment isn't hosted on Cloudflare".into()))?;
    let conn = super::connection(state, row.workspace_id).await?.ok_or_else(|| {
        Failed("Cloudflare is not connected to this workspace: connect it in Settings → Integrations".into())
    })?;
    let account = conn.metadata.account_id.clone().ok_or_else(|| {
        Failed("the Cloudflare token can't see an account; reconnect with one".into())
    })?;
    let script = script_name(cf.script_name, &row.service_name, &row.environment_name);

    // The archive the node uploaded, checked against what it reported.
    let archive = output::file(state, dep);
    let want = sha256.to_owned();
    let dest = scratch.to_owned();
    let files = tokio::task::spawn_blocking(move || unpack(&archive, &want, &dest))
        .await
        .map_err(|e| Failed(format!("unpacking the build output: {e}")))??;
    if files.is_empty() {
        return Err(Failed(format!(
            "the output directory {} is empty: check the build command and output directory",
            cf.output_dir
        )));
    }
    let total: u64 = files.iter().map(|f| f.size).sum();
    event(
        state,
        dep,
        "publishing",
        &format!(
            "Uploading {} files ({}) to Cloudflare Worker {script}",
            files.len(),
            human_size(total)
        ),
    )
    .await?;

    let manifest: Vec<ManifestEntry> = files
        .iter()
        .map(|f| ManifestEntry {
            path: f.path.clone(),
            hash: f.hash.clone(),
            size: f.size,
        })
        .collect();
    let session = conn
        .client
        .assets_upload_session(&account, &script, &manifest)
        .await?;
    let types: HashMap<&str, &str> = files
        .iter()
        .map(|f| (f.hash.as_str(), workers::content_type(&f.path)))
        .collect();
    let mut completion = session.jwt.clone();
    let needed: usize = session.buckets.iter().map(Vec::len).sum();
    for bucket in &session.buckets {
        let mut data = Vec::with_capacity(bucket.len());
        for hash in bucket {
            if !types.contains_key(hash.as_str()) || hash.contains(['/', '\\', '.']) {
                return Err(Failed(format!(
                    "Cloudflare asked for an unknown file {hash}"
                )));
            }
            let bytes = tokio::fs::read(scratch.join(hash))
                .await
                .map_err(|e| Failed(format!("reading the build output: {e}")))?;
            data.push((hash.as_str(), bytes));
        }
        let parts: Vec<AssetFile<'_>> = data
            .iter()
            .map(|(h, b)| AssetFile {
                hash: h,
                content_type: types[h],
                data: b,
            })
            .collect();
        if let Some(jwt) = conn
            .client
            .upload_assets(&account, &session.jwt, &parts)
            .await?
        {
            completion = jwt;
        }
    }
    if needed < files.len() {
        event(
            state,
            dep,
            "publishing",
            &format!(
                "{} files unchanged since an earlier upload",
                files.len() - needed
            ),
        )
        .await?;
    }

    // Variables and secrets, exactly as Modula has them.
    let (bindings, skipped) = bindings(state, dep, row).await?;
    if !skipped.is_empty() {
        event(
            state,
            dep,
            "warning",
            &format!(
                "Not set on the Worker (build-time or reserved names): {}",
                skipped.join(", ")
            ),
        )
        .await?;
    }
    conn.client
        .put_assets_worker(&account, &script, &completion, cf.spa, &bindings)
        .await?;
    event(
        state,
        dep,
        "publishing",
        &format!(
            "Deployed Worker {script} ({} variables{})",
            bindings.len(),
            if cf.spa { ", single-page app" } else { "" }
        ),
    )
    .await?;

    let mut url = None;
    match conn.client.workers_subdomain(&account).await {
        Ok(Some(sub)) => {
            conn.client.enable_workers_dev(&account, &script).await?;
            url = Some(format!("https://{script}.{sub}.workers.dev"));
        }
        Ok(None) => {
            event(
                state,
                dep,
                "warning",
                "This Cloudflare account has no workers.dev subdomain yet; choose one in the Cloudflare dashboard (Workers & Pages) or add a custom domain",
            )
            .await?;
        }
        Err(e) => return Err(e.into()),
    }
    if let Some(custom) = attach_domains(
        state,
        dep,
        row.service_instance_id,
        &conn,
        &account,
        &script,
    )
    .await?
    {
        url = Some(custom);
    }
    Ok(url)
}

/// Attaches the instance's custom domains to the Worker. A domain whose zone
/// isn't in the Cloudflare account is skipped with a warning; Modula's own
/// routing records for it are removed first (Cloudflare writes the Worker's).
/// Returns the first attached domain as a URL.
async fn attach_domains(
    state: &AppState,
    dep: Id,
    instance: Id,
    conn: &super::Connection,
    account: &str,
    script: &str,
) -> Result<Option<String>, Failed> {
    let domains: Vec<(Id, String)> = sqlx::query_as(
        "SELECT id, hostname FROM domains WHERE service_instance_id = $1 AND kind = 'custom'
         ORDER BY created_at",
    )
    .bind(instance)
    .fetch_all(&state.db)
    .await
    .map_err(crate::error::AppError::from)?;
    let mut first = None;
    for (id, host) in domains {
        if host.starts_with("*.") {
            event(
                state,
                dep,
                "warning",
                &format!("{host}: Workers custom domains can't be wildcards; skipped"),
            )
            .await?;
            continue;
        }
        let zone = match conn.client.find_zone(&host).await {
            Ok(Some(z)) => z,
            Ok(None) => {
                event(
                    state,
                    dep,
                    "warning",
                    &format!("{host} is not in a zone of this Cloudflare account; skipped"),
                )
                .await?;
                continue;
            }
            Err(e) => {
                event(state, dep, "warning", &format!("{host}: {e}")).await?;
                continue;
            }
        };
        for kind in ["CNAME", "A", "AAAA"] {
            if let Err(e) = conn.client.delete_records(&zone.id, &host, kind).await {
                tracing::debug!(error = %e, %host, "cannot remove routing records");
            }
        }
        match conn
            .client
            .attach_workers_domain(account, script, &host, &zone.id)
            .await
        {
            Ok(_) => {
                sqlx::query(
                    "UPDATE domains SET status = 'verified', verified_at = coalesce(verified_at, now()),
                            last_error = NULL, checked_at = now()
                     WHERE id = $1",
                )
                .bind(id)
                .execute(&state.db)
                .await
                .map_err(crate::error::AppError::from)?;
                event(state, dep, "publishing", &format!("Serving {host}")).await?;
                first.get_or_insert(format!("https://{host}"));
            }
            Err(e) => {
                let e = Failed::from(e).0;
                event(state, dep, "warning", &format!("{host}: {e}")).await?;
            }
        }
    }
    Ok(first)
}

/// The deployment's variables as Worker bindings: sealed variables (and
/// credential-looking ones) as secrets, the rest as plain text. Returns the
/// names left out.
async fn bindings(
    state: &AppState,
    dep: Id,
    row: &Row,
) -> Result<(Vec<Binding>, Vec<String>), Failed> {
    let resolved = match &row.env_sealed {
        Some(s) => env::open_snapshot(&state.keys, &state.db, row.environment_id, dep, s).await?,
        None => env::EnvMap::new(),
    };
    let raw = match &row.vars_sealed {
        Some(s) => {
            env::open_vars_snapshot(&state.keys, &state.db, row.environment_id, dep, s).await?
        }
        None => env::RawVars::new(),
    };
    Ok(to_bindings(&resolved, &raw))
}

fn to_bindings(resolved: &env::EnvMap, raw: &env::RawVars) -> (Vec<Binding>, Vec<String>) {
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    for (name, value) in resolved {
        if name == "ASSETS"
            || name.starts_with("MODULA_")
            || BUILD_ONLY_PREFIXES.iter().any(|p| name.starts_with(p))
        {
            skipped.push(name.clone());
            continue;
        }
        let text = value.expose_secret().to_owned();
        let secret = raw.get(name).is_some_and(|v| v.sealed) || env::worth_masking(name, &text);
        out.push(if secret {
            Binding::Secret {
                name: name.clone(),
                text,
            }
        } else {
            Binding::Plain {
                name: name.clone(),
                text,
            }
        });
    }
    (out, skipped)
}

/// Marks the deployment active (serving `url`) or failed, if nothing
/// superseded it meanwhile.
async fn finish(
    state: &AppState,
    dep: Id,
    row: &Row,
    outcome: Result<Option<String>, String>,
) -> AppResult<()> {
    let mut tx = state.db.begin().await?;
    let still: Option<String> =
        sqlx::query_scalar("SELECT status FROM deployments WHERE id = $1 FOR UPDATE")
            .bind(dep)
            .fetch_optional(&mut *tx)
            .await?;
    if still.as_deref() != Some("deploying") {
        return Ok(());
    }
    let mut kick = Vec::new();
    let commit_state = match &outcome {
        Ok(url) => {
            sqlx::query("UPDATE deployments SET url = $2 WHERE id = $1")
                .bind(dep)
                .bind(url)
                .execute(&mut *tx)
                .await?;
            kick = crate::deploy::state::activate(&mut tx, row.service_instance_id, dep).await?;
            let msg = match url {
                Some(u) => format!("Deployment is live at {u}"),
                None => "Deployment is live".to_owned(),
            };
            sqlx::query(
                "INSERT INTO deployment_events (deployment_id, phase, message) VALUES ($1, 'active', $2)",
            )
            .bind(dep)
            .bind(msg)
            .execute(&mut *tx)
            .await?;
            "success"
        }
        Err(msg) => {
            let msg: String = msg.chars().take(2000).collect();
            sqlx::query(
                "UPDATE deployments SET status = 'failed', error = $2, finished_at = now(), updated_at = now()
                 WHERE id = $1",
            )
            .bind(dep)
            .bind(&msg)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO deployment_events (deployment_id, phase, message) VALUES ($1, 'failed', $2)",
            )
            .bind(dep)
            .bind(&msg)
            .execute(&mut *tx)
            .await?;
            "failure"
        }
    };
    let hook = if commit_state == "success" {
        crate::webhooks::Event::DeploymentSucceeded
    } else {
        crate::webhooks::Event::DeploymentFailed
    };
    crate::webhooks::enqueue(&mut tx, dep, hook).await?;
    if commit_state != "success" {
        let event = crate::alerts::Event {
            kind: crate::alerts::Kind::DeploymentFailed,
            service_instance_id: row.service_instance_id,
            deployment_id: Some(dep),
            backup_id: None,
        };
        crate::alerts::enqueue_event(&mut *tx, event).await?;
    }
    crate::github::report(&mut tx, dep, commit_state, row.reports_commit).await?;
    kick.sort();
    kick.dedup();
    for n in &kick {
        crate::deploy::bump(&mut *tx, *n).await?;
    }
    tx.commit().await?;
    state.hub.kick_all(&kick);
    Ok(())
}

/// One file of the output, stored in the scratch directory under its hash.
#[derive(Debug)]
struct OutputFile {
    /// `/assets/app.js`.
    path: String,
    hash: String,
    size: u64,
}

/// Checks the archive's SHA-256, then unpacks its regular files into `dest`
/// (named by content hash). The archive holds one top-level directory (the
/// output directory, as `docker cp` packs it). Links and special files are
/// skipped; paths leaving the directory are refused.
fn unpack(archive: &Path, want_sha256: &str, dest: &Path) -> Result<Vec<OutputFile>, Failed> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let io = |e: std::io::Error| Failed(format!("reading the build output: {e}"));
    let mut f = std::fs::File::open(archive).map_err(|_| {
        Failed("the build output never arrived from the server that built it".into())
    })?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(io)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    if hex::encode(h.finalize()) != want_sha256 {
        return Err(Failed(
            "the build output doesn't match what the server that built it reported".into(),
        ));
    }
    std::fs::create_dir_all(dest).map_err(io)?;
    let f = std::fs::File::open(archive).map_err(io)?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(f));
    let mut out: BTreeMap<String, OutputFile> = BTreeMap::new();
    for entry in tar.entries().map_err(io)? {
        let mut entry = entry.map_err(io)?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().map_err(io)?.into_owned();
        let Some(rel) = inside(&path) else {
            return Err(Failed(format!(
                "the build output contains an unsafe path: {}",
                path.display()
            )));
        };
        let Some(rel) = rel else { continue };
        let size = entry.size();
        if size > MAX_FILE_BYTES {
            return Err(Failed(format!(
                "{rel} is {}; Cloudflare serves files up to 25 MiB",
                human_size(size)
            )));
        }
        let mut data = Vec::with_capacity(size as usize);
        entry.read_to_end(&mut data).map_err(io)?;
        let hash = workers::asset_hash(&rel, &data);
        std::fs::write(dest.join(&hash), &data).map_err(io)?;
        out.insert(
            rel.clone(),
            OutputFile {
                path: rel,
                hash,
                size: data.len() as u64,
            },
        );
        if out.len() > MAX_FILES {
            return Err(Failed(format!(
                "the build output has more than {MAX_FILES} files"
            )));
        }
    }
    Ok(out.into_values().collect())
}

/// `dist/assets/a.js` → `Some(Some("/assets/a.js"))`: the path inside the
/// top-level directory. `Some(None)` for the directory itself; `None` for a
/// path that isn't plainly inside it.
fn inside(path: &Path) -> Option<Option<String>> {
    let mut parts = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(p) => parts.push(p.to_str()?.to_owned()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if parts.len() < 2 {
        return Some(None);
    }
    Some(Some(format!("/{}", parts[1..].join("/"))))
}

fn human_size(n: u64) -> String {
    match n {
        n if n >= 1024 * 1024 => format!("{:.1} MiB", n as f64 / 1024.0 / 1024.0),
        n if n >= 1024 => format!("{:.1} KiB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn script_names() {
        assert_eq!(script_name(None, "web", "production"), "web-production");
        assert_eq!(script_name(None, "Web App", "PR #12"), "web-app-pr-12");
        assert_eq!(script_name(Some("site"), "web", "production"), "site");
        assert_eq!(script_name(None, &"a".repeat(80), "x").len(), 63);
    }

    #[test]
    fn paths_stay_inside() {
        assert_eq!(
            inside(Path::new("dist/assets/a.js")),
            Some(Some("/assets/a.js".into()))
        );
        assert_eq!(
            inside(Path::new("./dist/index.html")),
            Some(Some("/index.html".into()))
        );
        assert_eq!(inside(Path::new("dist")), Some(None));
        assert_eq!(inside(Path::new("dist/../etc/passwd")), None);
        assert_eq!(inside(Path::new("/etc/passwd")), None);
    }

    #[test]
    fn variables_become_bindings() {
        let resolved: env::EnvMap = [
            ("API_URL", "https://api.example.com"),
            ("STRIPE_SECRET_KEY", "sk_live_abc123def456"),
            ("SEALED_ONE", "x"),
            ("VITE_API_URL", "https://api.example.com"),
            ("ASSETS", "nope"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned().into()))
        .collect();
        let mut raw = env::RawVars::new();
        raw.insert(
            "SEALED_ONE".into(),
            serde_json::from_value(json!({ "value": "x", "sealed": true })).unwrap(),
        );
        let (b, skipped) = to_bindings(&resolved, &raw);
        let kinds: Vec<(String, bool)> = b
            .iter()
            .map(|b| match b {
                Binding::Plain { name, .. } => (name.clone(), false),
                Binding::Secret { name, .. } => (name.clone(), true),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                ("API_URL".to_owned(), false),
                ("SEALED_ONE".to_owned(), true),
                ("STRIPE_SECRET_KEY".to_owned(), true),
            ]
        );
        assert_eq!(skipped, ["ASSETS", "VITE_API_URL"]);
    }
}
