//! modula-server: the Modula Deploy control plane.

pub mod activity;
pub mod api;
pub mod audit;
pub mod auth;
pub mod authz;
pub mod backup;
pub mod config;
pub mod db;
pub mod deploy;
pub mod domains;
pub mod error;
pub mod github;
pub mod http;
pub mod instance_domain;
pub mod integrations;
pub mod jobs;
pub mod keyring;
pub mod listen;
pub mod locations;
pub mod logs;
pub mod metrics;
pub mod nodes;
pub mod pki;
pub mod private;
pub mod provision;
pub mod ratelimit;
pub mod state;
pub mod sync;
pub mod templates;
pub mod tunnel;
pub mod updates;
pub mod validate;
pub mod webhooks;

use std::time::Duration;

use anyhow::Context;
use secrecy::ExposeSecret;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::keyring::Keyring;
use crate::ratelimit::RateLimiter;
use crate::state::AppState;

/// What `modula-server features` prints. install.sh writes units that rely
/// on a feature only when the release it installs has it, so upgrading to (or
/// rolling back to) an older release keeps working.
///
/// * `socket-activation`: takes its listeners from systemd (modula-server.socket).
/// * `upgrade-requests`: hands upgrades to modula-upgrade.path (`MODULA_UPGRADE_DIR`).
pub const FEATURES: &[&str] = &["socket-activation", "upgrade-requests"];

pub async fn serve(cfg: Config) -> anyhow::Result<()> {
    let kek = cfg.load_master_key()?;
    tracing::info!(kek_id = %kek.id(), "master key loaded");
    let db = db::connect(&cfg.database_url).await?;
    db::migrate(&db).await?;

    let redis = redis::Client::open(cfg.redis_url.as_str()).context("invalid MODULA_REDIS_URL")?;
    let redis = redis::aio::ConnectionManager::new(redis)
        .await
        .context("cannot connect to Redis")?;

    if let Some(hash) = auth::setup::preseeded_from_env()? {
        // Chosen by whoever installed this server (`modula server create`);
        // nothing to print, they finish setup with it.
        if auth::setup::preseed_if_pending(&db, hash).await? {
            eprintln!(
                "\n  Modula Deploy is not set up yet. It was installed with a preseeded setup token\n  ({}); finish setup with it at {}/setup.\n",
                auth::setup::PRESEEDED_HASH_ENV,
                cfg.public_url.as_str().trim_end_matches('/')
            );
        }
    } else if let Some(token) = auth::setup::issue_if_pending(&db).await? {
        // Printed to stderr only; the hash is all that is stored.
        eprintln!(
            "\n  Modula Deploy is not set up yet.\n  Open {} and use this one-time setup token:\n\n    {}\n\n  It expires in 24 hours and is replaced on every restart.\n",
            cfg.public_url,
            token.expose_secret()
        );
    }

    let listen = cfg.listen;
    // Sockets systemd holds across restarts (modula-server.socket), if any.
    let mut inherited = listen::Inherited::from_env()?;
    let state = AppState::new(db, cfg, Keyring::new(kek), RateLimiter::Redis(redis));
    instance_domain::load(&state).await?;
    let stop = state.shutdown.clone();
    let worker = tokio::spawn(jobs::run_worker(
        state.clone(),
        backup::register(jobs::Registry::default())
            .on("sessions.cleanup", |s, _| async move {
                let n = auth::session::delete_expired(&s.db).await?;
                tracing::info!(deleted = n, "expired sessions removed");
                api::github::prune(&s.db).await?;
                let purged = deploy::upload::purge(&s).await?;
                tracing::info!(purged, "expired source uploads removed");
                let samples = metrics::purge(&s.db).await?;
                tracing::info!(samples, "old metric samples removed");
                let lines = logs::purge(&s.db).await?;
                tracing::info!(lines, "old log lines removed");
                Ok(())
            })
            .on(github::app::STATUS_JOB, github::app::status_job)
            .on(domains::RESYNC_JOB, domains::resync_job)
            .on(webhooks::DISPATCH_JOB, webhooks::dispatch_job)
            .on(provision::track::TRACK_JOB, provision::track::track_job),
        format!("server-{}", modula_id::Id::new()),
        stop.clone(),
    ));
    let cleanup = tokio::spawn(schedule_cleanup(state.clone(), stop.clone()));
    let backups = tokio::spawn(backup::schedule_loop(state.clone(), stop.clone()));
    let domain_checks = tokio::spawn(domains::verify_loop(state.clone(), stop.clone()));
    let updates = tokio::spawn(updates::run(state.clone(), stop.clone()));
    tokio::spawn(private::startup(state.clone()));
    let rollout = tokio::spawn(updates::rollout::run(
        state.clone(),
        stop.clone(),
        updates::rollout::TICK,
    ));

    // Create the CA before accepting traffic so every replica sees the same one.
    let fingerprint = state
        .pki()
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?
        .fingerprint();
    tracing::info!(%fingerprint, "node CA ready");
    let node_listen = state.cfg.node_listen;
    let node_listener = inherited
        .listener(node_listen)
        .await
        .context("node endpoint")?;
    let node_endpoint = tokio::spawn({
        let (state, stop) = (state.clone(), stop.clone());
        async move {
            if let Err(e) = nodes::serve(state, node_listener, stop.clone()).await {
                tracing::error!(error = ?e, "node endpoint stopped");
                stop.cancel();
            }
        }
    });

    let listener = inherited.listener(listen).await?;
    if !inherited.is_empty() {
        tracing::warn!("systemd passed sockets this configuration doesn't use");
    }
    tracing::info!(%listen, "modula-server listening");
    tokio::spawn(shutdown_signal(stop.clone()));
    listen::serve_http(listener, http::app(state), stop.clone(), listen::DRAIN).await?;

    stop.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        let _ = worker.await;
        let _ = cleanup.await;
        let _ = backups.await;
        let _ = domain_checks.await;
        let _ = updates.await;
        let _ = rollout.await;
        let _ = node_endpoint.await;
    })
    .await;
    Ok(())
}

async fn schedule_cleanup(state: AppState, stop: CancellationToken) {
    let mut tick = tokio::time::interval(Duration::from_secs(3600));
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tick.tick() => {
                let job = jobs::NewJob {
                    kind: "sessions.cleanup",
                    dedupe_key: Some("sessions.cleanup"),
                    ..Default::default()
                };
                match jobs::enqueue(&state.db, job).await {
                    Ok(_) => state.jobs_notify.notify_one(),
                    Err(e) => tracing::warn!(error = %e, "could not schedule session cleanup"),
                }
            }
        }
    }
}

async fn shutdown_signal(stop: CancellationToken) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
        _ = stop.cancelled() => {}
    }
    tracing::info!("shutting down");
    stop.cancel();
}
