//! Config as code: a project described by `.modula/modula.ts`,
//! `.modula/modula.toml` or `.modula/modula.json`. [`model`] parses and
//! validates it, [`script`] evaluates a `modula.ts`, [`merge`] applies
//! environment overlays, [`remote`] reads what the server has, [`diff`]
//! computes the plan and [`pull`] writes a file from the server.
//! `commands::config` runs them.

pub mod diff;
pub mod merge;
pub mod model;
pub mod pull;
pub mod remote;
pub mod script;
pub mod typescript;
