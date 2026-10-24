//! modula-node: the daemon on every server. It joins the control plane once
//! (`modula-node join`), then keeps one outbound mTLS stream open and
//! reconciles the server's containers and functions to the desired state it
//! receives.

pub mod access;
pub mod backup;
pub mod build;
pub mod cron;
pub mod db;
pub mod disk;
pub mod edge;
pub mod exec;
pub mod functions;
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
