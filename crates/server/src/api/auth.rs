use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum_extra::extract::CookieJar;
use modula_crypto::{ct_eq, password, token};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::audit::Entry;
use crate::auth::session::{self, Actor, SessionKind};
use crate::authz::Role;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::ratelimit::Limit;
use crate::state::AppState;
use crate::validate;

const LOGIN_PER_IP: Limit = Limit::per_minute(20);
const LOGIN_PER_EMAIL: Limit = Limit::per(10, std::time::Duration::from_secs(15 * 60));
const SETUP_PER_IP: Limit = Limit::per(10, std::time::Duration::from_secs(15 * 60));

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeUser {
    pub id: Uuid,
    pub email: String,
    pub name: String,
    pub is_instance_admin: bool,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Membership {
    pub id: Uuid,
    pub name: String,
    pub role: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Me {
    pub user: MeUser,
    pub workspaces: Vec<Membership>,
}

pub async fn load_me(db: &PgPool, user_id: Uuid) -> AppResult<Me> {
    let (id, email, name, is_instance_admin): (Uuid, String, String, bool) =
        sqlx::query_as("SELECT id, email, name, is_instance_admin FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(db)
            .await?;
    let workspaces = sqlx::query_as(
        "SELECT w.id, w.name, m.role FROM workspace_members m
         JOIN workspaces w ON w.id = m.workspace_id
         WHERE m.user_id = $1 ORDER BY w.created_at",
    )
    .bind(user_id)
    .fetch_all(db)
    .await?;
    Ok(Me {
        user: MeUser {
            id,
            email,
            name,
            is_instance_admin,
        },
        workspaces,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SetupRequest {
    pub setup_token: String,
    pub email: String,
    pub name: String,
    pub password: String,
    pub workspace_name: Option<String>,
}

pub async fn setup(
    State(state): State<AppState>,
    meta: RequestMeta,
    jar: CookieJar,
    Json(req): Json<SetupRequest>,
) -> AppResult<(CookieJar, Json<Me>)> {
    state
        .limiter
        .check("setup", &meta.ip_string(), SETUP_PER_IP)
        .await?;
    let email = validate::email(&req.email)?;
    let name = validate::display_name(&req.name, "name")?;
    let workspace_name = validate::display_name(
        req.workspace_name.as_deref().unwrap_or("Personal"),
        "workspace name",
    )?;
    validate::password(&req.password)?;
    let pw = req.password.clone();
    let password_hash = tokio::task::spawn_blocking(move || password::hash(&pw))
        .await
        .map_err(|e| AppError::Internal(e.into()))??;

    let mut tx = state.db.begin().await?;
    let row: (Option<Vec<u8>>, Option<OffsetDateTime>, Option<OffsetDateTime>) = sqlx::query_as(
        "SELECT setup_token_hash, setup_token_expires_at, setup_completed_at FROM instance FOR UPDATE",
    )
    .fetch_one(&mut *tx)
    .await?;
    let (stored_hash, expires_at, completed_at) = row;
    if completed_at.is_some() {
        return Err(AppError::NotFound);
    }
    let presented = token::hash(req.setup_token.trim());
    let valid = token::is_well_formed(req.setup_token.trim(), token::prefix::SETUP)
        && stored_hash
            .as_deref()
            .is_some_and(|h| ct_eq(h, presented.as_bytes()))
        && expires_at.is_some_and(|e| e > OffsetDateTime::now_utc());
    if !valid {
        Entry::new("instance.setup_failed", &meta)
            .record(&mut *tx)
            .await?;
        tx.commit().await?;
        return Err(AppError::InvalidCredentials);
    }

    let user_id = Uuid::now_v7();
    let workspace_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (id, email, name, password_hash, is_instance_admin) VALUES ($1, $2, $3, $4, true)",
    )
    .bind(user_id)
    .bind(&email)
    .bind(&name)
    .bind(&password_hash)
    .execute(&mut *tx)
    .await?;
    create_workspace_tx(&mut tx, workspace_id, &workspace_name, user_id).await?;
    sqlx::query(
        "UPDATE instance SET setup_completed_at = now(), setup_token_hash = NULL, setup_token_expires_at = NULL",
    )
    .execute(&mut *tx)
    .await?;
    Entry::new("instance.setup_completed", &meta)
        .user(user_id)
        .workspace(workspace_id)
        .record(&mut *tx)
        .await?;
    tx.commit().await?;

    let s = session::create(&state.db, user_id, SessionKind::Full, &meta).await?;
    let jar = jar.add(session::set_cookie(&state.cfg, SessionKind::Full, &s));
    Ok((jar, Json(load_me(&state.db, user_id).await?)))
}

pub async fn create_workspace_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
    name: &str,
    admin: Uuid,
) -> AppResult<()> {
    sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, $2)")
        .bind(id)
        .bind(name)
        .execute(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(admin)
        .bind(Role::Admin.as_str())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResponse {
    /// Set when a second factor is required before a full session is issued (C0b).
    pub mfa_required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub me: Option<Me>,
}

pub async fn login(
    State(state): State<AppState>,
    meta: RequestMeta,
    headers: HeaderMap,
    jar: CookieJar,
    Json(req): Json<LoginRequest>,
) -> AppResult<(CookieJar, Json<LoginResponse>)> {
    state
        .limiter
        .check("login-ip", &meta.ip_string(), LOGIN_PER_IP)
        .await?;
    // Malformed emails still count against the per-email bucket under their raw form.
    let email = validate::email(&req.email).unwrap_or_else(|_| req.email.trim().to_lowercase());
    state
        .limiter
        .check("login-email", &email, LOGIN_PER_EMAIL)
        .await?;

    let user: Option<(Uuid, Option<String>)> =
        sqlx::query_as("SELECT id, password_hash FROM users WHERE email = $1")
            .bind(&email)
            .fetch_optional(&state.db)
            .await?;
    let pw = req.password;
    let (user_id, ok) = match user {
        Some((id, Some(hash))) => {
            let ok = tokio::task::spawn_blocking(move || password::verify(&pw, &hash))
                .await
                .map_err(|e| AppError::Internal(e.into()))?;
            (Some(id), ok)
        }
        _ => {
            tokio::task::spawn_blocking(move || password::verify_dummy(&pw))
                .await
                .map_err(|e| AppError::Internal(e.into()))?;
            (None, false)
        }
    };
    let Some(user_id) = user_id.filter(|_| ok) else {
        let mut e = Entry::new("auth.login_failed", &meta).metadata(json!({ "email": email }));
        if let Some(id) = user_id {
            e = e.user(id);
        }
        e.record(&state.db).await?;
        return Err(AppError::InvalidCredentials);
    };

    // Rotation: never keep a pre-login session alive (prevents fixation).
    if let Some(old) = session::read_cookie(&headers, SessionKind::Full.cookie_name(&state.cfg)) {
        session::revoke_presented(&state.db, &old).await?;
    }
    let s = session::create(&state.db, user_id, SessionKind::Full, &meta).await?;
    Entry::new("auth.login", &meta)
        .user(user_id)
        .target("session", s.id)
        .record(&state.db)
        .await?;
    let jar = jar.add(session::set_cookie(&state.cfg, SessionKind::Full, &s));
    let me = load_me(&state.db, user_id).await?;
    Ok((
        jar,
        Json(LoginResponse {
            mfa_required: false,
            me: Some(me),
        }),
    ))
}

pub async fn logout(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
    jar: CookieJar,
) -> AppResult<(CookieJar, Json<serde_json::Value>)> {
    session::revoke(&state.db, actor.session_id).await?;
    Entry::new("auth.logout", &meta)
        .actor(&actor)
        .record(&state.db)
        .await?;
    Ok((
        jar.add(session::clear_cookie(&state.cfg, SessionKind::Full)),
        Json(json!({ "ok": true })),
    ))
}

pub async fn me(State(state): State<AppState>, actor: Actor) -> AppResult<Json<Me>> {
    Ok(Json(load_me(&state.db, actor.user_id).await?))
}
