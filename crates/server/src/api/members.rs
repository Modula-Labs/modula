//! Workspace members and invite links. Invites are the only way into someone
//! else's workspace: an admin creates a single-use link (the token is shown
//! once and stored hashed), and whoever opens it joins with the link's role,
//! signing up first if they have no account. The product sends no email.

use axum::Json;
use axum::extract::State;
use axum_extra::extract::CookieJar;
use modula_crypto::{password, token};
use modula_id::Id;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::{Duration, OffsetDateTime};

use super::auth::{Me, load_me};
use crate::audit::Entry;
use crate::auth::session::{self, Actor, SessionKind};
use crate::authz::{Grant, Role};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::ratelimit::Limit;
use crate::state::AppState;
use crate::validate;

pub const DEFAULT_INVITE_DAYS: i64 = 7;
const MAX_INVITE_DAYS: i64 = 30;
/// Looking up, accepting and signing up through invite links, per IP.
const INVITE_PER_IP: Limit = Limit::per(30, std::time::Duration::from_secs(15 * 60));
const LAST_ADMIN: &str = "a workspace needs at least one admin";

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    pub id: Id,
    pub user_id: Id,
    pub name: String,
    pub email: String,
    pub role: String,
    #[serde(with = "time::serde::rfc3339")]
    pub joined_at: OffsetDateTime,
    pub two_factor: bool,
}

macro_rules! member_select {
    () => {
        "SELECT m.id, m.user_id, u.name, u.email, m.role,
            m.created_at AS joined_at, u.totp_enabled_at IS NOT NULL AS two_factor
         FROM workspace_members m JOIN users u ON u.id = m.user_id"
    };
}

pub async fn list(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Vec<Member>>> {
    let ws = grant.workspace_id()?;
    let members = sqlx::query_as(concat!(
        member_select!(),
        " WHERE m.workspace_id = $1 ORDER BY m.created_at, m.id"
    ))
    .bind(ws)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(members))
}

async fn fetch_member(state: &AppState, id: Id) -> AppResult<Member> {
    sqlx::query_as(concat!(member_select!(), " WHERE m.id = $1"))
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)
}

/// Locks the workspace's memberships and returns `(user_id, role)` of the one
/// with id `member`, plus how many admins the workspace has.
async fn lock_member(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ws: Id,
    member: Id,
) -> AppResult<(Id, Role, usize)> {
    let rows: Vec<(Id, Id, String)> = sqlx::query_as(
        "SELECT id, user_id, role FROM workspace_members WHERE workspace_id = $1 FOR UPDATE",
    )
    .bind(ws)
    .fetch_all(&mut **tx)
    .await?;
    let admins = rows
        .iter()
        .filter(|(_, _, r)| r == Role::Admin.as_str())
        .count();
    let (_, user, role) = rows
        .into_iter()
        .find(|(id, _, _)| *id == member)
        .ok_or(AppError::NotFound)?;
    let role = Role::parse(&role)
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("unknown role {role}")))?;
    Ok((user, role, admins))
}

/// Deletes the membership and the user's API tokens limited to the workspace.
/// Sessions and unrestricted tokens stay: every request re-checks membership.
async fn delete_member_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ws: Id,
    user: Id,
) -> AppResult<u64> {
    sqlx::query("DELETE FROM workspace_members WHERE workspace_id = $1 AND user_id = $2")
        .bind(ws)
        .bind(user)
        .execute(&mut **tx)
        .await?;
    let tokens = sqlx::query("DELETE FROM api_tokens WHERE user_id = $1 AND workspace_id = $2")
        .bind(user)
        .bind(ws)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    Ok(tokens)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateMember {
    pub role: String,
}

fn parse_role(raw: &str) -> AppResult<Role> {
    Role::parse(raw)
        .ok_or_else(|| AppError::bad_request("role must be one of admin, member, deployer, viewer"))
}

pub async fn update(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<UpdateMember>,
) -> AppResult<Json<Member>> {
    let ws = grant.workspace_id()?;
    let member = grant.id();
    let role = parse_role(&req.role)?;
    let mut tx = state.db.begin().await?;
    let (user, old, admins) = lock_member(&mut tx, ws, member).await?;
    if old == Role::Admin && role != Role::Admin && admins <= 1 {
        return Err(AppError::Conflict(LAST_ADMIN.into()));
    }
    sqlx::query("UPDATE workspace_members SET role = $2 WHERE id = $1")
        .bind(member)
        .bind(role.as_str())
        .execute(&mut *tx)
        .await?;
    Entry::new("member.update", &meta)
        .actor(&actor)
        .workspace(ws)
        .target("user", user)
        .metadata(json!({ "from": old.as_str(), "to": role.as_str() }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    fetch_member(&state, member).await.map(Json)
}

pub async fn remove(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<serde_json::Value>> {
    let ws = grant.workspace_id()?;
    let mut tx = state.db.begin().await?;
    let (user, role, admins) = lock_member(&mut tx, ws, grant.id()).await?;
    if role == Role::Admin && admins <= 1 {
        return Err(AppError::Conflict(LAST_ADMIN.into()));
    }
    let tokens = delete_member_tx(&mut tx, ws, user).await?;
    Entry::new("member.remove", &meta)
        .actor(&actor)
        .workspace(ws)
        .target("user", user)
        .metadata(json!({ "role": role.as_str(), "tokensRevoked": tokens }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn leave(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<serde_json::Value>> {
    let ws = grant.workspace_id()?;
    let mut tx = state.db.begin().await?;
    let member: Id = sqlx::query_scalar(
        "SELECT id FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(ws)
    .bind(actor.user_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let (user, role, admins) = lock_member(&mut tx, ws, member).await?;
    if role == Role::Admin && admins <= 1 {
        return Err(AppError::Conflict(LAST_ADMIN.into()));
    }
    let tokens = delete_member_tx(&mut tx, ws, user).await?;
    Entry::new("member.leave", &meta)
        .actor(&actor)
        .workspace(ws)
        .target("user", user)
        .metadata(json!({ "role": role.as_str(), "tokensRevoked": tokens }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({ "ok": true })))
}

// ------------------------------------------------------------------ invites

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Invite {
    pub id: Id,
    pub role: String,
    pub created_by_name: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

macro_rules! invite_select {
    () => {
        "SELECT i.id, i.role, u.name AS created_by_name, i.created_at, i.expires_at
         FROM workspace_invites i LEFT JOIN users u ON u.id = i.created_by"
    };
}

/// Unused, unrevoked and unexpired.
macro_rules! pending {
    () => {
        "i.used_at IS NULL AND i.revoked_at IS NULL AND i.expires_at > now()"
    };
}

pub async fn list_invites(
    State(state): State<AppState>,
    grant: Grant,
) -> AppResult<Json<Vec<Invite>>> {
    let ws = grant.workspace_id()?;
    let invites = sqlx::query_as(concat!(
        invite_select!(),
        " WHERE i.workspace_id = $1 AND ",
        pending!(),
        " ORDER BY i.created_at DESC"
    ))
    .bind(ws)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(invites))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateInvite {
    pub role: String,
    pub expires_in_days: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedInvite {
    pub invite: Invite,
    /// The link to share; the only time the token is returned.
    pub url: String,
}

pub async fn create_invite(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<CreateInvite>,
) -> AppResult<Json<CreatedInvite>> {
    let ws = grant.workspace_id()?;
    let role = parse_role(&req.role)?;
    // An admin invite grants what promoting a member to admin does.
    if role == Role::Admin && !actor.sudo_active() {
        return Err(AppError::SudoRequired);
    }
    let days = req.expires_in_days.unwrap_or(DEFAULT_INVITE_DAYS);
    if !(1..=MAX_INVITE_DAYS).contains(&days) {
        return Err(AppError::bad_request(format!(
            "expiresInDays must be between 1 and {MAX_INVITE_DAYS}"
        )));
    }
    let t = token::generate(token::prefix::INVITE)?;
    let id = Id::new();
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "INSERT INTO workspace_invites (id, workspace_id, role, token_hash, created_by, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(ws)
    .bind(role.as_str())
    .bind(t.hash.as_bytes())
    .bind(actor.user_id)
    .bind(OffsetDateTime::now_utc() + Duration::days(days))
    .execute(&mut *tx)
    .await?;
    Entry::new("invite.create", &meta)
        .actor(&actor)
        .workspace(ws)
        .target("invite", id)
        .metadata(json!({ "role": role.as_str(), "expiresInDays": days }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    let invite = sqlx::query_as(concat!(invite_select!(), " WHERE i.id = $1"))
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    let url = state
        .public_url()
        .join(&format!("invite/{}", t.plaintext.expose_secret()))
        .map_err(|e| AppError::Internal(e.into()))?
        .to_string();
    Ok(Json(CreatedInvite { invite, url }))
}

pub async fn revoke_invite(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<serde_json::Value>> {
    let ws = grant.workspace_id()?;
    let id = grant.id();
    let mut tx = state.db.begin().await?;
    let revoked = sqlx::query(
        "UPDATE workspace_invites SET revoked_at = now()
         WHERE id = $1 AND used_at IS NULL AND revoked_at IS NULL",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if revoked == 0 {
        return Err(AppError::Conflict(
            "this invite was already used or revoked".into(),
        ));
    }
    Entry::new("invite.revoke", &meta)
        .actor(&actor)
        .workspace(ws)
        .target("invite", id)
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InviteToken {
    pub token: String,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct InvitePreview {
    pub workspace_name: String,
    pub role: String,
    pub invited_by: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

async fn limit(state: &AppState, meta: &RequestMeta) -> AppResult<()> {
    state
        .limiter
        .check("invite", &meta.ip_string(), INVITE_PER_IP)
        .await
}

/// The token's hash, or `NotFound` when it can't be an invite token.
fn token_hash(raw: &str) -> AppResult<token::TokenHash> {
    let raw = raw.trim();
    if !token::is_well_formed(raw, token::prefix::INVITE) {
        return Err(AppError::NotFound);
    }
    Ok(token::hash(raw))
}

/// What the invite page shows before joining. Used, revoked and expired
/// links are indistinguishable from unknown ones.
pub async fn lookup(
    State(state): State<AppState>,
    meta: RequestMeta,
    Json(req): Json<InviteToken>,
) -> AppResult<Json<InvitePreview>> {
    limit(&state, &meta).await?;
    let hash = token_hash(&req.token)?;
    sqlx::query_as(concat!(
        "SELECT w.name AS workspace_name, i.role, u.name AS invited_by, i.expires_at
         FROM workspace_invites i
         JOIN workspaces w ON w.id = i.workspace_id
         LEFT JOIN users u ON u.id = i.created_by
         WHERE i.token_hash = $1 AND ",
        pending!()
    ))
    .bind(hash.as_bytes())
    .fetch_optional(&state.db)
    .await?
    .map(Json)
    .ok_or(AppError::NotFound)
}

/// Marks the invite used by `user` and adds them to its workspace with its
/// role. The conditional update is what makes a link single-use.
async fn redeem_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    hash: &token::TokenHash,
    user: Id,
) -> AppResult<(Id, Id, String)> {
    let (invite, ws, role): (Id, Id, String) = sqlx::query_as(concat!(
        "UPDATE workspace_invites i SET used_at = now(), used_by = $2
         WHERE i.token_hash = $1 AND ",
        pending!(),
        "
         RETURNING i.id, i.workspace_id, i.role"
    ))
    .bind(hash.as_bytes())
    .bind(user)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let added = sqlx::query(
        "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, $3)
         ON CONFLICT DO NOTHING",
    )
    .bind(ws)
    .bind(user)
    .bind(&role)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if added == 0 {
        return Err(AppError::Conflict(
            "you are already a member of this workspace".into(),
        ));
    }
    Ok((invite, ws, role))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Joined {
    pub workspace_id: Id,
    pub me: Me,
}

/// A signed-in user joins through an invite link.
pub async fn accept(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
    Json(req): Json<InviteToken>,
) -> AppResult<Json<Joined>> {
    // A token limited to one workspace can't join another.
    if actor.workspace_scope().is_some() {
        return Err(AppError::Forbidden);
    }
    limit(&state, &meta).await?;
    let hash = token_hash(&req.token)?;
    let mut tx = state.db.begin().await?;
    let (invite, ws, role) = redeem_tx(&mut tx, &hash, actor.user_id).await?;
    Entry::new("member.join", &meta)
        .actor(&actor)
        .workspace(ws)
        .target("invite", invite)
        .metadata(json!({ "role": role }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(Joined {
        workspace_id: ws,
        me: load_me(&state.db, actor.user_id).await?,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InviteSignup {
    pub token: String,
    pub name: String,
    pub email: String,
    pub password: String,
}

/// Someone without an account creates one and joins through an invite link.
pub async fn signup(
    State(state): State<AppState>,
    meta: RequestMeta,
    jar: CookieJar,
    Json(req): Json<InviteSignup>,
) -> AppResult<(CookieJar, Json<Joined>)> {
    limit(&state, &meta).await?;
    let hash = token_hash(&req.token)?;
    let email = validate::email(&req.email)?;
    let name = validate::display_name(&req.name, "name")?;
    validate::password(&req.password)?;
    let pw = req.password;
    let password_hash = tokio::task::spawn_blocking(move || password::hash(&pw))
        .await
        .map_err(|e| AppError::Internal(e.into()))??;

    let user = Id::new();
    let mut tx = state.db.begin().await?;
    // The link is checked first, so only its holder learns whether an
    // email already has an account.
    sqlx::query(concat!(
        "SELECT 1 FROM workspace_invites i WHERE i.token_hash = $1 AND ",
        pending!(),
        " FOR UPDATE"
    ))
    .bind(hash.as_bytes())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let created = sqlx::query(
        "INSERT INTO users (id, email, name, password_hash) VALUES ($1, $2, $3, $4)
         ON CONFLICT (email) DO NOTHING",
    )
    .bind(user)
    .bind(&email)
    .bind(&name)
    .bind(&password_hash)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if created == 0 {
        return Err(AppError::Conflict(
            "an account with this email already exists; sign in to accept the invite".into(),
        ));
    }
    let (invite, ws, role) = redeem_tx(&mut tx, &hash, user).await?;
    Entry::new("auth.signup", &meta)
        .user(user)
        .workspace(ws)
        .target("invite", invite)
        .record(&mut *tx)
        .await?;
    Entry::new("member.join", &meta)
        .user(user)
        .workspace(ws)
        .target("invite", invite)
        .metadata(json!({ "role": role }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;

    let s = session::create(&state.db, user, SessionKind::Full, &meta).await?;
    let jar = jar.add(session::set_cookie(&state.cfg, SessionKind::Full, &s));
    Ok((
        jar,
        Json(Joined {
            workspace_id: ws,
            me: load_me(&state.db, user).await?,
        }),
    ))
}
