//! WebAuthn passkeys as a second factor (webauthn-rs, no attestation).
//!
//! The relying party is the dashboard's domain: WebAuthn needs a hostname,
//! so an instance reached only at its IP can't use passkeys. A ceremony's
//! state lives in `webauthn_challenges`, bound to the session that started
//! it, and is consumed by the step that finishes it.

use modula_id::Id;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use url::Url;
use webauthn_rs::prelude::{
    CreationChallengeResponse, Passkey, PasskeyAuthentication, PasskeyRegistration,
    PublicKeyCredential, RegisterPublicKeyCredential, RequestChallengeResponse, Uuid, Webauthn,
    WebauthnBuilder,
};

use crate::auth::mfa::ISSUER;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// How long the browser has to answer a challenge.
const CHALLENGE_TTL: Duration = Duration::minutes(5);

/// The relying party for the dashboard's current address: its domain once
/// set, else the install URL when that is a hostname (`localhost` in
/// development). None when the dashboard only has an IP.
pub fn relying_party(state: &AppState) -> Option<(String, Url)> {
    let url = state.public_url();
    let host = url.domain()?.to_owned();
    let origin = url.origin().ascii_serialization().parse().ok()?;
    Some((host, origin))
}

fn webauthn(state: &AppState) -> AppResult<(Webauthn, String)> {
    let (rp_id, origin) = relying_party(state).ok_or_else(|| {
        AppError::bad_request("passkeys need the dashboard to have a domain; set one in Settings")
    })?;
    let w = WebauthnBuilder::new(&rp_id, &origin)
        .and_then(|b| b.rp_name(ISSUER).build())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("webauthn config: {e:?}")))?;
    Ok((w, rp_id))
}

/// The WebAuthn user handle: stable per user, not the id itself.
fn user_handle(user_id: Id) -> Uuid {
    let d = Sha256::digest(format!("modula-webauthn-user:{user_id}"));
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    Uuid::from_bytes(b)
}

#[derive(Clone, Copy)]
enum Purpose {
    Register,
    Authenticate,
}

impl Purpose {
    fn as_str(self) -> &'static str {
        match self {
            Self::Register => "register",
            Self::Authenticate => "authenticate",
        }
    }
}

async fn put_challenge<T: Serialize>(
    db: &PgPool,
    session_id: Id,
    purpose: Purpose,
    state: &T,
) -> AppResult<()> {
    let json = serde_json::to_value(state).map_err(|e| AppError::Internal(e.into()))?;
    sqlx::query(
        "INSERT INTO webauthn_challenges (session_id, purpose, state, expires_at)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (session_id, purpose)
         DO UPDATE SET state = EXCLUDED.state, expires_at = EXCLUDED.expires_at",
    )
    .bind(session_id)
    .bind(purpose.as_str())
    .bind(json)
    .bind(OffsetDateTime::now_utc() + CHALLENGE_TTL)
    .execute(db)
    .await?;
    Ok(())
}

/// Take (and delete) the session's pending challenge; None if missing or expired.
async fn take_challenge<T: DeserializeOwned>(
    db: &PgPool,
    session_id: Id,
    purpose: Purpose,
) -> AppResult<Option<T>> {
    let row: Option<(serde_json::Value, OffsetDateTime)> = sqlx::query_as(
        "DELETE FROM webauthn_challenges WHERE session_id = $1 AND purpose = $2
         RETURNING state, expires_at",
    )
    .bind(session_id)
    .bind(purpose.as_str())
    .fetch_optional(db)
    .await?;
    Ok(match row {
        Some((state, expires)) if expires > OffsetDateTime::now_utc() => {
            Some(serde_json::from_value(state).map_err(|e| AppError::Internal(e.into()))?)
        }
        _ => None,
    })
}

pub async fn count(db: &PgPool, user_id: Id) -> AppResult<i64> {
    Ok(
        sqlx::query_scalar("SELECT count(*) FROM passkeys WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(db)
            .await?,
    )
}

/// Whether the user has a passkey that can sign in at the dashboard's address.
pub async fn usable_here(state: &AppState, user_id: Id) -> AppResult<bool> {
    let Some((rp_id, _)) = relying_party(state) else {
        return Ok(false);
    };
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM passkeys WHERE user_id = $1 AND rp_id = $2)",
    )
    .bind(user_id)
    .bind(rp_id)
    .fetch_one(&state.db)
    .await?)
}

/// The user's passkeys registered for `rp_id` (the only ones that can sign in).
async fn usable(db: &PgPool, user_id: Id, rp_id: &str) -> AppResult<Vec<(Id, Passkey)>> {
    let rows: Vec<(Id, serde_json::Value)> = sqlx::query_as(
        "SELECT id, credential FROM passkeys WHERE user_id = $1 AND rp_id = $2 ORDER BY created_at",
    )
    .bind(user_id)
    .bind(rp_id)
    .fetch_all(db)
    .await?;
    rows.into_iter()
        .map(|(id, c)| {
            serde_json::from_value(c)
                .map(|p| (id, p))
                .map_err(|e| AppError::Internal(e.into()))
        })
        .collect()
}

/// Start registering a passkey. Existing ones are excluded so an
/// authenticator can't be added twice.
pub async fn begin_registration(
    state: &AppState,
    user_id: Id,
    session_id: Id,
) -> AppResult<CreationChallengeResponse> {
    let (w, _) = webauthn(state)?;
    let (email, name): (String, String) =
        sqlx::query_as("SELECT email, name FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&state.db)
            .await?;
    let existing: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT credential_id FROM passkeys WHERE user_id = $1")
            .bind(user_id)
            .fetch_all(&state.db)
            .await?;
    let exclude = (!existing.is_empty()).then_some(existing);
    let (ccr, reg) = w
        .start_passkey_registration(user_handle(user_id), &email, &name, exclude)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("passkey registration: {e:?}")))?;
    put_challenge(&state.db, session_id, Purpose::Register, &reg).await?;
    Ok(ccr)
}

/// Verify the browser's answer and store the passkey; returns its id.
pub async fn finish_registration(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Id,
    session_id: Id,
    name: &str,
    credential: &RegisterPublicKeyCredential,
) -> AppResult<Id> {
    let (w, rp_id) = webauthn(state)?;
    let reg: PasskeyRegistration = take_challenge(&state.db, session_id, Purpose::Register)
        .await?
        .ok_or_else(|| AppError::bad_request("no passkey registration in progress; start again"))?;
    let passkey = w
        .finish_passkey_registration(credential, &reg)
        .map_err(|_| AppError::bad_request("the passkey could not be verified"))?;
    let id = Id::new();
    let inserted = sqlx::query(
        "INSERT INTO passkeys (id, user_id, credential_id, credential, rp_id, name)
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (credential_id) DO NOTHING",
    )
    .bind(id)
    .bind(user_id)
    .bind(passkey.cred_id().to_vec())
    .bind(serde_json::to_value(&passkey).map_err(|e| AppError::Internal(e.into()))?)
    .bind(&rp_id)
    .bind(name)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if inserted != 1 {
        return Err(AppError::Conflict(
            "this passkey is already registered".into(),
        ));
    }
    Ok(id)
}

/// Challenge the user's usable passkeys (login second step or sudo).
pub async fn begin_authentication(
    state: &AppState,
    user_id: Id,
    session_id: Id,
) -> AppResult<RequestChallengeResponse> {
    let (w, rp_id) = webauthn(state)?;
    let keys: Vec<Passkey> = usable(&state.db, user_id, &rp_id)
        .await?
        .into_iter()
        .map(|(_, p)| p)
        .collect();
    if keys.is_empty() {
        return Err(AppError::bad_request(
            "no passkey for this dashboard address; use another method",
        ));
    }
    let (rcr, auth) = w
        .start_passkey_authentication(&keys)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("passkey authentication: {e:?}")))?;
    put_challenge(&state.db, session_id, Purpose::Authenticate, &auth).await?;
    Ok(rcr)
}

/// Verify an assertion against the session's challenge. A wrong answer
/// burns the challenge (the client starts a new one).
pub async fn verify(
    state: &AppState,
    user_id: Id,
    session_id: Id,
    credential: &PublicKeyCredential,
) -> AppResult<bool> {
    let Ok((w, rp_id)) = webauthn(state) else {
        return Ok(false);
    };
    let Some(auth): Option<PasskeyAuthentication> =
        take_challenge(&state.db, session_id, Purpose::Authenticate).await?
    else {
        return Ok(false);
    };
    let Ok(result) = w.finish_passkey_authentication(credential, &auth) else {
        return Ok(false);
    };
    // The challenge only allowed this user's keys, but check ownership anyway.
    let Some((id, mut passkey)) = usable(&state.db, user_id, &rp_id)
        .await?
        .into_iter()
        .find(|(_, p)| p.cred_id() == result.cred_id())
    else {
        return Ok(false);
    };
    passkey.update_credential(&result);
    sqlx::query("UPDATE passkeys SET credential = $2, last_used_at = now() WHERE id = $1")
        .bind(id)
        .bind(serde_json::to_value(&passkey).map_err(|e| AppError::Internal(e.into()))?)
        .execute(&state.db)
        .await?;
    Ok(true)
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct PasskeyInfo {
    pub id: Id,
    pub name: String,
    pub rp_id: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_used_at: Option<OffsetDateTime>,
}

pub async fn list(db: &PgPool, user_id: Id) -> AppResult<Vec<PasskeyInfo>> {
    Ok(sqlx::query_as(
        "SELECT id, name, rp_id, created_at, last_used_at FROM passkeys
         WHERE user_id = $1 ORDER BY created_at",
    )
    .bind(user_id)
    .fetch_all(db)
    .await?)
}
