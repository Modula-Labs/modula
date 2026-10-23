//! Config as code: a project described by `modula.toml` at the repository
//! root. [`model`] parses and validates it, [`merge`] applies environment
//! overlays, [`remote`] reads what the server has, [`diff`] computes the
//! plan and [`pull`] writes a file from the server. `commands::config` runs
//! them.

pub mod diff;
pub mod merge;
pub mod model;
pub mod pull;
pub mod remote;
