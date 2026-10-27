//! Moving services between servers with their data: one service instance to
//! another existing server ([`moves`]), or everything on a worker server to a
//! new server created for it ([`server`]). Volume data travels node →
//! control plane → node over the nodes' streams ([`transfer`]).
//!
//! Migrating the main server (the one serving the dashboard) also moves the
//! control plane ([`promote`], with [`maintenance`] meanwhile).

pub mod api;
pub mod maintenance;
pub mod moves;
pub mod promote;
pub mod server;
pub mod transfer;

pub use transfer::Transfers;
