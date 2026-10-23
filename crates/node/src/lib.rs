//! modula-node: the daemon on every server. It joins the control plane once
//! (`modula-node join`), then keeps one outbound mTLS stream open and
//! reconciles the server's containers to the desired state it receives.

pub mod build;
pub mod edge;
pub mod identity;
pub mod join;
pub mod location;
pub mod reconcile;
pub mod runtime;
pub mod session;
pub mod tls;

/// Reported to the server in Hello and Enroll.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
