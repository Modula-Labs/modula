//! First-run setup token. Until setup completes, the only way to create the
//! first (admin) account is with a one-time token printed to the server's log,
//! so nobody can race the operator to the signup page.

use modula_crypto::token::{self, prefix};
use secrecy::SecretString;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};

use crate::error::AppResult;

pub const TTL: Duration = Duration::hours(24);

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
