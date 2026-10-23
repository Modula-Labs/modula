//! Listeners: inherited from systemd (socket activation) or bound here.
//!
//! The installer runs `modula-server.socket`, which holds the API port
//! (MODULA_LISTEN, behind Caddy) and the node port (MODULA_NODE_LISTEN) and
//! passes them in `LISTEN_FDS`. Because systemd keeps them open while the
//! service restarts, an upgrade never refuses a connection: new ones wait in
//! the kernel's backlog until the new process accepts them, while the old one
//! finishes what it already accepted ([`serve_http`]).
//!
//! Each inherited socket is matched to its role by port, so the order of
//! `ListenStream=` lines doesn't matter. Without socket activation (development,
//! older units) the server binds the addresses itself.

use std::future::IntoFuture;
use std::net::SocketAddr;
use std::os::fd::{FromRawFd, RawFd};
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use tokio_util::sync::CancellationToken;

/// The first descriptor systemd passes (`SD_LISTEN_FDS_START`).
const FIRST_FD: RawFd = 3;

/// How many descriptors systemd passed to this process: `LISTEN_FDS`, but only
/// when `LISTEN_PID` names us (the variables are inherited by children too).
pub fn passed_fds(listen_pid: Option<&str>, listen_fds: Option<&str>, pid: u32) -> usize {
    match (listen_pid, listen_fds) {
        (Some(p), Some(n)) if p.trim().parse::<u32>().ok() == Some(pid) => {
            n.trim().parse::<usize>().unwrap_or(0).min(64)
        }
        _ => 0,
    }
}

/// Listening sockets passed by systemd, not yet claimed.
#[derive(Default)]
pub struct Inherited {
    listeners: Vec<std::net::TcpListener>,
}

impl Inherited {
    /// Takes over the descriptors systemd passed to this process (none when
    /// not socket-activated).
    pub fn from_env() -> anyhow::Result<Self> {
        let n = passed_fds(
            std::env::var("LISTEN_PID").ok().as_deref(),
            std::env::var("LISTEN_FDS").ok().as_deref(),
            std::process::id(),
        );
        let fds = (FIRST_FD..).take(n);
        // SAFETY: systemd hands these descriptors to this process (LISTEN_PID
        // is ours) for it to own; nothing else in the process uses them.
        Self::adopt(fds.map(|fd| unsafe { std::net::TcpListener::from_raw_fd(fd) }))
    }

    /// Adopts already-open listeners (tests use this directly). Each is
    /// re-created close-on-exec, so processes the server runs (pg_dump) don't
    /// inherit the ports.
    pub fn adopt(
        listeners: impl IntoIterator<Item = std::net::TcpListener>,
    ) -> anyhow::Result<Self> {
        let mut out = Vec::new();
        for l in listeners {
            let addr = l
                .local_addr()
                .context("an inherited descriptor is not a listening socket")?;
            // try_clone duplicates with F_DUPFD_CLOEXEC; the original closes on drop.
            let l = l
                .try_clone()
                .context("cannot take over an inherited socket")?;
            l.set_nonblocking(true)?;
            tracing::info!(%addr, "using a socket passed by systemd");
            out.push(l);
        }
        Ok(Self { listeners: out })
    }

    pub fn is_empty(&self) -> bool {
        self.listeners.is_empty()
    }

    /// The inherited socket for `addr` (same port), if there is one.
    pub fn take(&mut self, addr: SocketAddr) -> Option<std::net::TcpListener> {
        let i = self
            .listeners
            .iter()
            .position(|l| l.local_addr().is_ok_and(|a| a.port() == addr.port()))?;
        Some(self.listeners.swap_remove(i))
    }

    /// The inherited socket for `addr`, or a new one bound to it.
    pub async fn listener(&mut self, addr: SocketAddr) -> anyhow::Result<tokio::net::TcpListener> {
        match self.take(addr) {
            Some(l) => Ok(tokio::net::TcpListener::from_std(l)?),
            None => tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("cannot bind {addr}")),
        }
    }
}

/// How long in-flight requests may run on after shutdown begins. Streams
/// (deployment logs, node sessions) end right away on `AppState::shutdown`;
/// this bounds ordinary requests, which time out after 30 s anyway.
pub const DRAIN: Duration = Duration::from_secs(20);

/// Serves `app` until `stop` is cancelled, then stops accepting and lets
/// in-flight requests finish for at most `drain`.
pub async fn serve_http(
    listener: tokio::net::TcpListener,
    app: Router,
    stop: CancellationToken,
    drain: Duration,
) -> anyhow::Result<()> {
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(stop.clone().cancelled_owned())
    .into_future();
    tokio::select! {
        r = server => r?,
        _ = async { stop.cancelled().await; tokio::time::sleep(drain).await } => {
            tracing::warn!(?drain, "requests still running after the drain period; stopping anyway");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_fds_only_for_this_process() {
        assert_eq!(passed_fds(Some("42"), Some("2"), 42), 2);
        assert_eq!(passed_fds(Some(" 42\n"), Some("2"), 42), 2);
        // Inherited from a parent that was socket-activated: not ours.
        assert_eq!(passed_fds(Some("41"), Some("2"), 42), 0);
        assert_eq!(passed_fds(None, Some("2"), 42), 0);
        assert_eq!(passed_fds(Some("42"), None, 42), 0);
        assert_eq!(passed_fds(Some("42"), Some("x"), 42), 0);
        assert_eq!(passed_fds(Some("42"), Some("100000"), 42), 64);
    }

    #[tokio::test]
    async fn inherited_sockets_are_matched_by_port() {
        let a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let b = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (a_addr, b_addr) = (a.local_addr().unwrap(), b.local_addr().unwrap());
        // Passed in the "wrong" order.
        let mut inherited = Inherited::adopt([b, a]).unwrap();
        let wanted: SocketAddr = format!("0.0.0.0:{}", a_addr.port()).parse().unwrap();
        let l = inherited.listener(wanted).await.unwrap();
        assert_eq!(l.local_addr().unwrap(), a_addr);
        assert!(inherited.take(a_addr).is_none(), "claimed once");
        assert_eq!(
            inherited.take(b_addr).unwrap().local_addr().unwrap(),
            b_addr
        );
        assert!(inherited.is_empty());
        // Nothing inherited for this one: bound here.
        let own = inherited
            .listener("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        assert_ne!(own.local_addr().unwrap().port(), 0);
    }
}
