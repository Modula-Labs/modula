//! modula-server: the Modula Deploy control plane.

pub mod api;
pub mod audit;
pub mod auth;
pub mod authz;
pub mod config;
pub mod db;
pub mod deploy;
pub mod error;
pub mod http;
pub mod jobs;
pub mod keyring;
pub mod nodes;
pub mod pki;
pub mod ratelimit;
pub mod state;
pub mod validate;

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context;
use secrecy::ExposeSecret;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::keyring::Keyring;
use crate::ratelimit::RateLimiter;
use crate::state::AppState;

pub async fn serve(cfg: Config) -> anyhow::Result<()> {
    let kek = cfg.load_master_key()?;
    tracing::info!(kek_id = %kek.id(), "master key loaded");
    let db = db::connect(&cfg.database_url).await?;
    db::migrate(&db).await?;

    let redis = redis::Client::open(cfg.redis_url.as_str()).context("invalid MODULA_REDIS_URL")?;
    let redis = redis::aio::ConnectionManager::new(redis)
        .await
        .context("cannot connect to Redis")?;

    if let Some(token) = auth::setup::issue_if_pending(&db).await? {
        // Printed to stderr only; the hash is all that is stored.
        eprintln!(
            "\n  Modula Deploy is not set up yet.\n  Open {} and use this one-time setup token:\n\n    {}\n\n  It expires in 24 hours and is replaced on every restart.\n",
            cfg.public_url,
            token.expose_secret()
        );
    }

    let listen = cfg.listen;
    let state = AppState::new(db, cfg, Keyring::new(kek), RateLimiter::Redis(redis));
    let stop = CancellationToken::new();
    let worker = tokio::spawn(jobs::run_worker(
        state.clone(),
        jobs::Registry::default().on("sessions.cleanup", |s, _| async move {
            let n = auth::session::delete_expired(&s.db).await?;
            tracing::info!(deleted = n, "expired sessions removed");
            Ok(())
        }),
        format!("server-{}", uuid::Uuid::new_v4()),
        stop.clone(),
    ));
    let cleanup = tokio::spawn(schedule_cleanup(state.clone(), stop.clone()));

    // Create the CA before accepting traffic so every replica sees the same one.
    let fingerprint = state
        .pki()
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?
        .fingerprint();
    tracing::info!(%fingerprint, "node CA ready");
    let node_listen = state.cfg.node_listen;
    let node_listener = tokio::net::TcpListener::bind(node_listen)
        .await
        .with_context(|| format!("cannot bind node endpoint {node_listen}"))?;
    let node_endpoint = tokio::spawn({
        let (state, stop) = (state.clone(), stop.clone());
        async move {
            if let Err(e) = nodes::serve(state, node_listener, stop.clone()).await {
                tracing::error!(error = ?e, "node endpoint stopped");
                stop.cancel();
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot bind {listen}"))?;
    tracing::info!(%listen, "modula-server listening");
    axum::serve(
        listener,
        http::app(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(stop.clone()))
    .await?;

    stop.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        let _ = worker.await;
        let _ = cleanup.await;
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
}
