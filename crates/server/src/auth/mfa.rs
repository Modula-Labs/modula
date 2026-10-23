//! Second factors: TOTP (with replay protection) and single-use backup codes.

use modula_crypto::envelope::{FieldAad, INSTANCE_SCOPE};
use modula_crypto::totp::{self, backup};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub const ISSUER: &str = "Modula Deploy";
/// An unconfirmed enrollment must be confirmed within this window.
const ENROLL_TTL: Duration = Duration::minutes(15);

fn row_id(user_id: Uuid) -> String {
    user_id.to_string()
}

fn aad<'a>(column: &'a str, row: &'a str) -> FieldAad<'a> {
    FieldAad {
        scope: INSTANCE_SCOPE,
        table: "users",
        column,
        row,
    }
}

fn now_unix() -> u64 {
    OffsetDateTime::now_utc().unix_timestamp().max(0) as u64
}

async fn pepper(state: &AppState) -> AppResult<[u8; 32]> {
    Ok(state
        .keys
        .data_key(&state.db, INSTANCE_SCOPE)
        .await?
        .derive("backup-codes"))
}

pub async fn totp_enabled(db: &PgPool, user_id: Uuid) -> AppResult<bool> {
    Ok(
        sqlx::query_scalar("SELECT totp_enabled_at IS NOT NULL FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(db)
            .await?,
    )
}

/// Verify a TOTP code for an enrolled user and consume its time step.
pub async fn verify_totp(state: &AppState, user_id: Uuid, code: &str) -> AppResult<bool> {
    let row: Option<(Option<Vec<u8>>, Option<i64>)> =
        sqlx::query_as("SELECT totp_secret, totp_last_step FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(&state.db)
            .await?;
    let Some((Some(sealed), last)) = row else {
        return Ok(false);
    };
    let rid = row_id(user_id);
    let secret = state
        .keys
        .open(&state.db, &sealed, aad("totp_secret", &rid))
        .await?;
    let Some(step) = totp::verify(&secret, code, now_unix(), last.map(|s| s as u64))? else {
        return Ok(false);
    };
    // Compare-and-set: two concurrent requests with the same code cannot both win.
    let won = sqlx::query(
        "UPDATE users SET totp_last_step = $2
         WHERE id = $1 AND (totp_last_step IS NULL OR totp_last_step < $2)",
    )
    .bind(user_id)
    .bind(step as i64)
    .execute(&state.db)
    .await?
    .rows_affected()
        == 1;
    Ok(won)
}

pub async fn consume_backup_code(state: &AppState, user_id: Uuid, code: &str) -> AppResult<bool> {
    let Some(normalized) = backup::normalize(code) else {
        return Ok(false);
    };
    let hash = backup::hash(&pepper(state).await?, &normalized);
    let used = sqlx::query(
        "UPDATE backup_codes SET used_at = now()
         WHERE user_id = $1 AND code_hash = $2 AND used_at IS NULL",
    )
    .bind(user_id)
    .bind(&hash[..])
    .execute(&state.db)
    .await?
    .rows_affected()
        == 1;
    Ok(used)
}

pub async fn backup_codes_remaining(db: &PgPool, user_id: Uuid) -> AppResult<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM backup_codes WHERE user_id = $1 AND used_at IS NULL",
    )
    .bind(user_id)
    .fetch_one(db)
    .await?)
}

pub struct Enrollment {
    pub otpauth_url: Zeroizing<String>,
    pub secret_base32: Zeroizing<String>,
}

/// Start (or restart) enrollment. The secret stays pending until confirmed.
pub async fn begin_enrollment(
    state: &AppState,
    user_id: Uuid,
    email: &str,
) -> AppResult<Enrollment> {
    if totp_enabled(&state.db, user_id).await? {
        return Err(AppError::Conflict(
            "two-factor authentication is already enabled".into(),
        ));
    }
    let secret = totp::generate_secret()?;
    let rid = row_id(user_id);
    let sealed = state
        .keys
        .seal(&state.db, &secret, aad("totp_pending_secret", &rid))
        .await?;
    sqlx::query(
        "UPDATE users SET totp_pending_secret = $2, totp_pending_created_at = now() WHERE id = $1",
    )
    .bind(user_id)
    .bind(&sealed)
    .execute(&state.db)
    .await?;
    Ok(Enrollment {
        otpauth_url: totp::otpauth_url(&secret, ISSUER, email),
        secret_base32: totp::secret_base32(&secret),
    })
}

/// Confirm with a code from the app; enables TOTP and returns fresh backup codes.
pub async fn confirm_enrollment(
    state: &AppState,
    user_id: Uuid,
    code: &str,
) -> AppResult<Vec<Zeroizing<String>>> {
    let row: (Option<Vec<u8>>, Option<OffsetDateTime>) = sqlx::query_as(
        "SELECT totp_pending_secret, totp_pending_created_at FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_one(&state.db)
    .await?;
    let (Some(pending), Some(created)) = row else {
        return Err(AppError::bad_request(
            "no two-factor enrollment in progress",
        ));
    };
    if OffsetDateTime::now_utc() - created > ENROLL_TTL {
        return Err(AppError::bad_request("enrollment expired; start again"));
    }
    let rid = row_id(user_id);
    let secret = state
        .keys
        .open(&state.db, &pending, aad("totp_pending_secret", &rid))
        .await?;
    let Some(step) = totp::verify(&secret, code, now_unix(), None)? else {
        return Err(AppError::bad_request("invalid code"));
    };
    // Re-seal under the active column so the pending ciphertext can't be swapped in.
    let active = state
        .keys
        .seal(&state.db, &secret, aad("totp_secret", &rid))
        .await?;
    let mut tx = state.db.begin().await?;
    let updated = sqlx::query(
        "UPDATE users SET totp_secret = $2, totp_enabled_at = now(), totp_last_step = $3,
                totp_pending_secret = NULL, totp_pending_created_at = NULL, updated_at = now()
         WHERE id = $1 AND totp_enabled_at IS NULL",
    )
    .bind(user_id)
    .bind(&active)
    .bind(step as i64)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "two-factor authentication is already enabled".into(),
        ));
    }
    let codes = replace_backup_codes(state, &mut tx, user_id).await?;
    tx.commit().await?;
    Ok(codes)
}

pub async fn disable(state: &AppState, user_id: Uuid) -> AppResult<()> {
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "UPDATE users SET totp_secret = NULL, totp_enabled_at = NULL, totp_last_step = NULL,
                totp_pending_secret = NULL, totp_pending_created_at = NULL, updated_at = now()
         WHERE id = $1",
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM backup_codes WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn regenerate_backup_codes(
    state: &AppState,
    user_id: Uuid,
) -> AppResult<Vec<Zeroizing<String>>> {
    if !totp_enabled(&state.db, user_id).await? {
        return Err(AppError::bad_request(
            "enable two-factor authentication first",
        ));
    }
    let mut tx = state.db.begin().await?;
    let codes = replace_backup_codes(state, &mut tx, user_id).await?;
    tx.commit().await?;
    Ok(codes)
}

async fn replace_backup_codes(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
) -> AppResult<Vec<Zeroizing<String>>> {
    let pepper = pepper(state).await?;
    sqlx::query("DELETE FROM backup_codes WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    let codes = backup::generate()?;
    for c in &codes {
        let n = backup::normalize(c).expect("generated codes are valid");
        sqlx::query("INSERT INTO backup_codes (user_id, code_hash) VALUES ($1, $2)")
            .bind(user_id)
            .bind(&backup::hash(&pepper, &n)[..])
            .execute(&mut **tx)
            .await?;
    }
    Ok(codes)
}
