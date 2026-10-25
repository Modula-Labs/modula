//! `modula login` through the browser, shaped like OAuth 2.0 device
//! authorization (RFC 8628):
//!
//! 1. The CLI asks for a login (`POST /api/cli/login`) and gets a secret
//!    `device_code` plus a short `user_code` and the dashboard URL to open.
//! 2. The user opens `/cli/login?code=…` in the dashboard, signed in, checks
//!    the code and the machine's name, and approves (sudo) or denies.
//! 3. The CLI polls `POST /api/cli/login/token` with its `device_code`; once
//!    approved, that poll mints an ordinary API token (`CLI on <host>`,
//!    listed and revocable with the user's other tokens) and returns it.
//!
//! Only the device code's hash is stored, a login is collected at most once
//! (approved -> consumed in one statement), and both codes expire after
//! [`TTL`]. Knowing the user code (it is shown on screen) never yields a
//! token: polling needs the device code.

use axum::Json;
use axum::extract::State;
use modula_crypto::token::{self, prefix};
use modula_id::Id;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::ratelimit::Limit;
use crate::state::AppState;

/// How long a login waits for approval.
pub const TTL: time::Duration = time::Duration::minutes(10);
/// Seconds the CLI waits between polls.
pub const INTERVAL_SECS: u64 = 2;

/// The public endpoints, called by the CLI without a session or Origin
/// (see `http::middleware`).
pub const START_PATH: &str = "/api/cli/login";
pub const TOKEN_PATH: &str = "/api/cli/login/token";

const START_PER_IP: Limit = Limit::per(20, std::time::Duration::from_secs(15 * 60));
const POLL_PER_IP: Limit = Limit::per_minute(120);
/// Lookups, approvals and denials: guessing user codes needs a session, and
/// is slow even then.
const REVIEW_PER_USER: Limit = Limit::per(30, std::time::Duration::from_secs(10 * 60));

/// Letters without vowels (no words) or look-alikes, as RFC 8628 suggests.
const ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";
const USER_CODE_LEN: usize = 8;

/// A random user code, normalized (no dash).
fn new_user_code() -> AppResult<String> {
    let mut out = String::with_capacity(USER_CODE_LEN);
    while out.len() < USER_CODE_LEN {
        for b in modula_crypto::random_bytes::<16>()? {
            // 240 = 12 * 20: rejecting the rest keeps every letter equally likely.
            if b < 240 && out.len() < USER_CODE_LEN {
                out.push(ALPHABET[usize::from(b) % ALPHABET.len()] as char);
            }
        }
    }
    Ok(out)
}

/// `BCDFGHJK` -> `BCDF-GHJK`.
pub fn display_code(code: &str) -> String {
    format!("{}-{}", &code[..4], &code[4..])
}

/// What a person typed (any case, dash or spaces optional), normalized; `None`
/// when it can't be a user code.
pub fn normalize_code(raw: &str) -> Option<String> {
    let code: String = raw
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    (code.len() == USER_CODE_LEN && code.bytes().all(|b| ALPHABET.contains(&b))).then_some(code)
}

/// The machine name the CLI reports, reduced to a safe label for the token
/// name and the approval page.
pub fn clean_hostname(raw: Option<&str>) -> String {
    let h: String = raw
        .unwrap_or_default()
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .take(48)
        .collect();
    if h.is_empty() {
        "unknown host".into()
    } else {
        h
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct StartRequest {
    /// The machine `modula login` runs on.
    pub hostname: Option<String>,
}

#[derive(Serialize)]
pub struct StartResponse {
    pub device_code: String,
    /// `XXXX-XXXX`, shown in the terminal and on the approval page.
    pub user_code: String,
    /// The dashboard page that approves this login (code included).
    pub verification_url: String,
    pub interval: u64,
    pub expires_in: i64,
}

pub async fn start(
    State(state): State<AppState>,
    meta: RequestMeta,
    body: Option<Json<StartRequest>>,
) -> AppResult<Json<StartResponse>> {
    state
        .limiter
        .check("cli_login_start", &meta.ip_string(), START_PER_IP)
        .await?;
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let hostname = clean_hostname(req.hostname.as_deref());
    sqlx::query("DELETE FROM cli_logins WHERE expires_at < now() - interval '1 hour'")
        .execute(&state.db)
        .await?;
    let device = token::generate(prefix::CLI_LOGIN)?;
    let expires_at = OffsetDateTime::now_utc() + TTL;
    // A clash with a live code is astronomically rare; try again then.
    let mut user_code = None;
    for _ in 0..5 {
        let code = new_user_code()?;
        let inserted = sqlx::query(
            "INSERT INTO cli_logins (id, device_code_hash, user_code, hostname, ip, user_agent, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (user_code) DO NOTHING",
        )
        .bind(Id::new())
        .bind(device.hash.as_bytes())
        .bind(&code)
        .bind(&hostname)
        .bind(meta.ip_string())
        .bind(meta.user_agent.as_deref())
        .bind(expires_at)
        .execute(&state.db)
        .await?
        .rows_affected();
        if inserted == 1 {
            user_code = Some(code);
            break;
        }
    }
    let code = user_code.ok_or_else(|| AppError::Internal(anyhow::anyhow!("no free user code")))?;
    let mut url = state.public_url();
    url.set_path("/cli/login");
    url.set_query(Some(&format!("code={}", display_code(&code))));
    Ok(Json(StartResponse {
        device_code: device.plaintext.expose_secret().to_owned(),
        user_code: display_code(&code),
        verification_url: url.to_string(),
        interval: INTERVAL_SECS,
        expires_in: TTL.whole_seconds(),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollRequest {
    pub device_code: String,
}

#[derive(sqlx::FromRow)]
struct Collected {
    id: Id,
    hostname: String,
    approved_by: Id,
}

/// `{"status": "pending" | "denied" | "expired"}`, or `"approved"` with the
/// new `token` exactly once. Unknown and already collected codes read as
/// expired.
pub async fn poll(
    State(state): State<AppState>,
    meta: RequestMeta,
    Json(req): Json<PollRequest>,
) -> AppResult<Json<Value>> {
    state
        .limiter
        .check("cli_login_poll", &meta.ip_string(), POLL_PER_IP)
        .await?;
    let expired = || Ok(Json(json!({ "status": "expired" })));
    if !token::is_well_formed(&req.device_code, prefix::CLI_LOGIN) {
        return expired();
    }
    let hash = token::hash(&req.device_code);
    let mut tx = state.db.begin().await?;
    let collected: Option<Collected> = sqlx::query_as(
        "UPDATE cli_logins SET status = 'consumed'
         WHERE device_code_hash = $1 AND status = 'approved' AND expires_at > now()
         RETURNING id, hostname, approved_by",
    )
    .bind(hash.as_bytes())
    .fetch_optional(&mut *tx)
    .await?;
    let Some(c) = collected else {
        tx.rollback().await?;
        let status: Option<String> = sqlx::query_scalar(
            "SELECT status FROM cli_logins WHERE device_code_hash = $1 AND expires_at > now()",
        )
        .bind(hash.as_bytes())
        .fetch_optional(&state.db)
        .await?;
        return match status.as_deref() {
            Some("pending") => Ok(Json(json!({ "status": "pending" }))),
            Some("denied") => Ok(Json(json!({ "status": "denied" }))),
            _ => expired(),
        };
    };
    let name = format!("CLI on {}", c.hostname);
    let (token_id, plaintext) =
        super::tokens::insert(&mut tx, c.approved_by, None, &name, None).await?;
    Entry::new("account.token_created", &meta)
        .user(c.approved_by)
        .target("api_token", token_id)
        .metadata(json!({ "name": name, "via": "cli_login", "cliLogin": c.id }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    let user: (String, String) = sqlx::query_as("SELECT email, name FROM users WHERE id = $1")
        .bind(c.approved_by)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(json!({
        "status": "approved",
        "token": plaintext,
        "user": { "email": user.0, "name": user.1 },
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodeRequest {
    pub user_code: String,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct PendingLogin {
    #[serde(skip)]
    pub id: Id,
    pub hostname: String,
    pub ip: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// A pending, unexpired login by its user code (404 otherwise).
async fn pending(state: &AppState, actor: &Actor, raw: &str) -> AppResult<(String, PendingLogin)> {
    state
        .limiter
        .check(
            "cli_login_review",
            &actor.user_id.to_string(),
            REVIEW_PER_USER,
        )
        .await?;
    let code = normalize_code(raw).ok_or(AppError::NotFound)?;
    let login: PendingLogin = sqlx::query_as(
        "SELECT id, hostname, ip, created_at, expires_at FROM cli_logins
         WHERE user_code = $1 AND status = 'pending' AND expires_at > now()",
    )
    .bind(&code)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok((code, login))
}

/// What the approval page shows: which machine asked, from where, when.
pub async fn lookup(
    State(state): State<AppState>,
    actor: Actor,
    Json(req): Json<CodeRequest>,
) -> AppResult<Json<Value>> {
    let (code, login) = pending(&state, &actor, &req.user_code).await?;
    let mut v = serde_json::to_value(&login).map_err(|e| AppError::Internal(e.into()))?;
    v["userCode"] = json!(display_code(&code));
    Ok(Json(v))
}

/// Requires sudo (a cookie session re-authenticated recently).
pub async fn approve(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
    Json(req): Json<CodeRequest>,
) -> AppResult<Json<Value>> {
    let (_, login) = pending(&state, &actor, &req.user_code).await?;
    if !super::tokens::below_cap(&state.db, actor.user_id).await? {
        return Err(AppError::Conflict(
            "you have too many API tokens; delete unused ones first".into(),
        ));
    }
    decide(&state, &actor, &meta, login, "approved").await
}

pub async fn deny(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
    Json(req): Json<CodeRequest>,
) -> AppResult<Json<Value>> {
    let (_, login) = pending(&state, &actor, &req.user_code).await?;
    decide(&state, &actor, &meta, login, "denied").await
}

async fn decide(
    state: &AppState,
    actor: &Actor,
    meta: &RequestMeta,
    login: PendingLogin,
    status: &'static str,
) -> AppResult<Json<Value>> {
    let mut tx = state.db.begin().await?;
    let changed = sqlx::query(
        "UPDATE cli_logins SET status = $2, approved_by = $3
         WHERE id = $1 AND status = 'pending' AND expires_at > now()",
    )
    .bind(login.id)
    .bind(status)
    .bind((status == "approved").then_some(actor.user_id))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed == 0 {
        return Err(AppError::NotFound);
    }
    let action = if status == "approved" {
        "account.cli_login_approved"
    } else {
        "account.cli_login_denied"
    };
    Entry::new(action, meta)
        .actor(actor)
        .target("cli_login", login.id)
        .metadata(json!({ "hostname": login.hostname, "ip": login.ip }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({ "ok": true, "status": status })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_codes_are_short_unambiguous_letters() {
        for _ in 0..200 {
            let c = new_user_code().unwrap();
            assert_eq!(c.len(), 8);
            assert_eq!(
                normalize_code(&display_code(&c)).as_deref(),
                Some(c.as_str())
            );
            assert_eq!(
                normalize_code(&c.to_lowercase()).as_deref(),
                Some(c.as_str())
            );
        }
        assert_eq!(normalize_code("bcdf ghjk").as_deref(), Some("BCDFGHJK"));
        assert_eq!(normalize_code("ABCD-EFGH"), None, "vowels never appear");
        assert_eq!(normalize_code("BCDF-GHJ"), None);
    }

    #[test]
    fn hostnames_are_reduced_to_a_safe_label() {
        assert_eq!(clean_hostname(Some("milans-mbp.local")), "milans-mbp.local");
        assert_eq!(clean_hostname(Some(" <b>evil</b>\n")), "bevilb");
        assert_eq!(clean_hostname(None), "unknown host");
        assert_eq!(clean_hostname(Some(&"x".repeat(100))).len(), 48);
    }
}
