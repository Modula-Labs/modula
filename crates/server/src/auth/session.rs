//! Cookie sessions backed by hashed opaque tokens.

use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use axum::http::request::Parts;
use axum_extra::extract::cookie::{Cookie, SameSite};
use modula_crypto::token::{self, prefix};
use secrecy::{ExposeSecret, SecretString};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;

pub const ABSOLUTE_TTL: Duration = Duration::days(30);
pub const IDLE_TTL: Duration = Duration::days(7);
pub const MFA_PENDING_TTL: Duration = Duration::minutes(10);
/// Don't write `last_seen_at` on every request.
const TOUCH_INTERVAL: Duration = Duration::minutes(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Full,
    MfaPending,
}

impl SessionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::MfaPending => "mfa_pending",
        }
    }
    fn prefix(self) -> &'static str {
        match self {
            Self::Full => prefix::SESSION,
            Self::MfaPending => prefix::MFA_PENDING,
        }
    }
    fn ttl(self) -> Duration {
        match self {
            Self::Full => ABSOLUTE_TTL,
            Self::MfaPending => MFA_PENDING_TTL,
        }
    }
    pub fn cookie_name(self, cfg: &Config) -> &'static str {
        match (self, cfg.secure_cookies()) {
            (Self::Full, true) => "__Host-modula_session",
            (Self::Full, false) => "modula_session",
            (Self::MfaPending, true) => "__Host-modula_mfa",
            (Self::MfaPending, false) => "modula_mfa",
        }
    }
}

/// The authenticated caller, inserted into request extensions by the router.
#[derive(Debug, Clone)]
pub struct Actor {
    pub user_id: Uuid,
    pub session_id: Uuid,
    pub is_instance_admin: bool,
    pub sudo_until: Option<OffsetDateTime>,
}

impl<S: Send + Sync> FromRequestParts<S> for Actor {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        // Present only on routes registered with a non-public `Access`.
        parts.extensions.get::<Actor>().cloned().ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!("Actor extractor used on a public route"))
        })
    }
}

pub struct NewSession {
    pub id: Uuid,
    pub token: SecretString,
    pub expires_at: OffsetDateTime,
}

pub async fn create(
    db: &PgPool,
    user_id: Uuid,
    kind: SessionKind,
    meta: &RequestMeta,
) -> AppResult<NewSession> {
    let t = token::generate(kind.prefix())?;
    let id = Uuid::now_v7();
    let expires_at = OffsetDateTime::now_utc() + kind.ttl();
    sqlx::query(
        "INSERT INTO sessions (id, user_id, token_hash, kind, ip, user_agent, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(user_id)
    .bind(t.hash.as_bytes())
    .bind(kind.as_str())
    .bind(meta.ip_string())
    .bind(&meta.user_agent)
    .bind(expires_at)
    .execute(db)
    .await?;
    Ok(NewSession {
        id,
        token: t.plaintext,
        expires_at,
    })
}

#[derive(sqlx::FromRow)]
struct SessionRow {
    id: Uuid,
    user_id: Uuid,
    is_instance_admin: bool,
    last_seen_at: OffsetDateTime,
    sudo_until: Option<OffsetDateTime>,
}

/// Resolve a presented token. Expired, idle, or wrong-kind sessions resolve to None.
pub async fn resolve(db: &PgPool, presented: &str, kind: SessionKind) -> AppResult<Option<Actor>> {
    if !token::is_well_formed(presented, kind.prefix()) {
        return Ok(None);
    }
    let hash = token::hash(presented);
    let now = OffsetDateTime::now_utc();
    let row: Option<SessionRow> = sqlx::query_as(
        "SELECT s.id, s.user_id, u.is_instance_admin, s.last_seen_at, s.sudo_until
         FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.token_hash = $1 AND s.kind = $2 AND s.expires_at > $3 AND s.last_seen_at > $4",
    )
    .bind(hash.as_bytes())
    .bind(kind.as_str())
    .bind(now)
    .bind(now - IDLE_TTL)
    .fetch_optional(db)
    .await?;
    let Some(row) = row else { return Ok(None) };
    if now - row.last_seen_at > TOUCH_INTERVAL {
        sqlx::query("UPDATE sessions SET last_seen_at = $2 WHERE id = $1")
            .bind(row.id)
            .bind(now)
            .execute(db)
            .await?;
    }
    Ok(Some(Actor {
        user_id: row.user_id,
        session_id: row.id,
        is_instance_admin: row.is_instance_admin,
        sudo_until: row.sudo_until,
    }))
}

pub async fn revoke(db: &PgPool, session_id: Uuid) -> AppResult<()> {
    sqlx::query("DELETE FROM sessions WHERE id = $1")
        .bind(session_id)
        .execute(db)
        .await?;
    Ok(())
}

/// Revoke whatever session the presented token belongs to (used for rotation on login).
pub async fn revoke_presented(db: &PgPool, presented: &str) -> AppResult<()> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
        .bind(token::hash(presented).as_bytes())
        .execute(db)
        .await?;
    Ok(())
}

pub async fn delete_expired(db: &PgPool) -> AppResult<u64> {
    let now = OffsetDateTime::now_utc();
    let res = sqlx::query("DELETE FROM sessions WHERE expires_at <= $1 OR last_seen_at <= $2")
        .bind(now)
        .bind(now - IDLE_TTL)
        .execute(db)
        .await?;
    Ok(res.rows_affected())
}

pub fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| Cookie::parse(pair.trim().to_owned()).ok())
        .find(|c| c.name() == name)
        .map(|c| c.value().to_owned())
}

pub fn set_cookie(cfg: &Config, kind: SessionKind, s: &NewSession) -> Cookie<'static> {
    let mut c = Cookie::build((kind.cookie_name(cfg), s.token.expose_secret().to_owned()))
        .path("/")
        .http_only(true)
        .secure(cfg.secure_cookies())
        .same_site(SameSite::Lax)
        .build();
    c.set_expires(s.expires_at);
    c
}

pub fn clear_cookie(cfg: &Config, kind: SessionKind) -> Cookie<'static> {
    let mut c = Cookie::build((kind.cookie_name(cfg), ""))
        .path("/")
        .http_only(true)
        .secure(cfg.secure_cookies())
        .same_site(SameSite::Lax)
        .build();
    c.make_removal();
    c
}
