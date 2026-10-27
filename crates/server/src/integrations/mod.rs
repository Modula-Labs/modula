//! Third-party provider integrations, connected per workspace. Tokens are
//! stored in the `integrations` table, sealed with the workspace data key.

pub mod cloudflare;
pub mod hetzner;
