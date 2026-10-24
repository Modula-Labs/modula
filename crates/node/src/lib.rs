//! modula-node: the daemon on every server. It joins the control plane once
//! (`modula-node join`), then keeps one outbound mTLS stream open and
//! reconciles the server's containers to the desired state it receives.

pub mod access;
pub mod backup;
pub mod build;
pub mod db;
pub mod edge;
pub mod identity;
pub mod join;
pub mod location;
pub mod logship;
pub mod mesh;
pub mod metrics;
pub mod reconcile;
pub mod runtime;
pub mod session;
pub mod tls;
pub mod tunnel;
pub mod upgrade;

/// Reported to the server in Hello and Enroll.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What `modula-node features` prints; install.sh only writes units that
/// rely on a feature when the release it installs has it (older releases keep
/// working with the units they expect).
pub const FEATURES: &[&str] = &["caddy-sockets", "upgrade"];
