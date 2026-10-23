//! First-run setup token. Until setup completes, the only way to create the
//! first (admin) account is with a one-time token printed to the server's log,
//! so nobody can race the operator to the signup page.

use modula_crypto::token::{self, TokenHash, prefix};
use secrecy::SecretString;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};

use crate::error::AppResult;

pub const TTL: Duration = Duration::hours(24);

/// `MODULA_SETUP_TOKEN_HASH`: the hex hash of a setup token chosen elsewhere
/// (`modula server create` generates one on the laptop and passes it to
/// install.sh as `--setup-token`; only its hash reaches the server). While
/// set, startup uses it instead of minting and printing a random token.
pub const PRESEEDED_HASH_ENV: &str = "MODULA_SETUP_TOKEN_HASH";

/// The preseeded token's hash from the environment, if any.
pub fn preseeded_from_env() -> anyhow::Result<Option<TokenHash>> {
    match std::env::var(PRESEEDED_HASH_ENV) {
        Ok(v) if !v.trim().is_empty() => parse_hash(v.trim()).map(Some),
        _ => Ok(None),
    }
}

fn parse_hash(hex_hash: &str) -> anyhow::Result<TokenHash> {
    let bytes = hex::decode(hex_hash)
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .ok_or_else(|| anyhow::anyhow!("{PRESEEDED_HASH_ENV} must be 64 hex characters"))?;
    Ok(TokenHash(bytes))
}

/// If setup is still pending, make `hash` the setup token (valid for [`TTL`]
/// from now). Idempotent, so restarts keep the same token. Returns whether
/// setup is pending.
pub async fn preseed_if_pending(db: &PgPool, hash: TokenHash) -> AppResult<bool> {
    let updated = sqlx::query(
        "UPDATE instance SET setup_token_hash = $1, setup_token_expires_at = $2
         WHERE setup_completed_at IS NULL",
    )
    .bind(hash.as_bytes())
    .bind(OffsetDateTime::now_utc() + TTL)
    .execute(db)
    .await?;
    Ok(updated.rows_affected() == 1)
}

/// If setup is still pending, mint a fresh token (invalidating any previous one)
/// and return it for display. Returns None once setup has completed.
pub async fn issue_if_pending(db: &PgPool) -> AppResult<Option<SecretString>> {
    let t = token::generate(prefix::SETUP)?;
    let updated = sqlx::query(
        "UPDATE instance SET setup_token_hash = $1, setup_token_expires_at = $2
         WHERE setup_completed_at IS NULL",
    )
    .bind(t.hash.as_bytes())
    .bind(OffsetDateTime::now_utc() + TTL)
    .execute(db)
    .await?;
    Ok((updated.rows_affected() == 1).then_some(t.plaintext))
}

pub async fn is_pending(db: &PgPool) -> AppResult<bool> {
    Ok(
        sqlx::query_scalar("SELECT setup_completed_at IS NULL FROM instance")
            .fetch_one(db)
            .await?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preseeded_hashes_are_64_hex() {
        let h = token::hash("mcsetup_x");
        assert_eq!(parse_hash(&hex::encode(h.0)).unwrap(), h);
        assert!(parse_hash("abc").is_err());
        assert!(parse_hash(&"zz".repeat(32)).is_err());
    }
}
