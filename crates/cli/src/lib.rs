//! `modula-deploy`, the Modula Deploy command line: everything the dashboard
//! does with an API token, from a terminal or an AI agent.
//!
//! [`cli`] defines the command line, [`commands`] runs it (one module per
//! command group) against the typed client in [`api`]; [`context`] resolves
//! the project, environment and service to act on.

pub mod api;
pub mod cli;
pub mod commands;
pub mod config;
pub mod connect;
pub mod context;
pub mod output;
pub mod resolve;
pub mod tunnel;
