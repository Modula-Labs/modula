//! HTTP handlers for the Cloudflare integration. Routes live in `api/mod.rs`.

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use super::client::{Account, Zone};
use super::{Client, Metadata};
use crate::api::backups;
use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Permission {
    pub name: &'static str,
    pub why: &'static str,
    pub required: bool,
}

/// The connection as clients see it: never the token, only its last four
/// characters.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub connected: bool,
    pub account_id: Option<String>,
    pub account_name: Option<String>,
    /// e.g. `…a1b2`.
    pub token_hint: Option<String>,
    pub connected_by: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,
    /// Pre-filled "create token" page with exactly the permissions below.
    pub token_template_url: String,
    pub permissions: Vec<Permission>,
}

fn permissions() -> Vec<Permission> {
    super::PERMISSIONS
        .iter()
        .map(|(name, why, required)| Permission {
            name,
            why,
            required: *required,
        })
        .collect()
}

async fn load_status(state: &AppState, workspace: Id) -> AppResult<Status> {
    let row: Option<(serde_json::Value, Option<String>, OffsetDateTime)> = sqlx::query_as(
        "SELECT i.metadata, u.name, i.updated_at FROM integrations i
         LEFT JOIN users u ON u.id = i.created_by
         WHERE i.workspace_id = $1 AND i.kind = 'cloudflare'",
    )
    .bind(workspace)
    .fetch_optional(&state.db)
    .await?;
    let mut status = Status {
        connected: false,
        account_id: None,
        account_name: None,
        token_hint: None,
        connected_by: None,
        updated_at: None,
        token_template_url: super::token_template_url(),
        permissions: permissions(),
    };
    if let Some((meta, by, at)) = row {
        let meta: Metadata = serde_json::from_value(meta).unwrap_or_default();
        status.connected = true;
        status.account_id = meta.account_id;
        status.account_name = meta.account_name;
        status.token_hint = Some(format!("…{}", meta.token_last4));
        status.connected_by = by;
        status.updated_at = Some(at);
    }
    Ok(status)
}

pub async fn status(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Status>> {
    Ok(Json(load_status(&state, grant.id()).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenRequest {
    pub token: String,
    /// Which account to use when the token sees several.
    #[serde(default)]
    pub account_id: Option<String>,
}

/// What a token can see, before it is stored.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub accounts: Vec<Account>,
    /// The account that will be used (None when the user must choose, or
    /// the token sees no account: R2 is then unavailable).
    pub account_id: Option<String>,
    pub zones: Vec<ZoneSummary>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZoneSummary {
    pub id: String,
    pub name: String,
    pub status: String,
    pub paused: bool,
}

impl From<Zone> for ZoneSummary {
    fn from(z: Zone) -> Self {
        Self {
            id: z.id,
            name: z.name,
            status: z.status,
            paused: z.paused,
        }
    }
}

struct Checked {
    token_id: String,
    accounts: Vec<Account>,
    account: Option<Account>,
    zones: Vec<Zone>,
}

/// Verifies the token with Cloudflare and finds what it can reach.
async fn check_token(client: &Client, wanted: Option<&str>) -> AppResult<Checked> {
    let verified = client.verify().await.map_err(|e| match e {
        e if e.is_forbidden() || e.status() == Some(400) => {
            AppError::bad_request(format!("Cloudflare rejected this token: {e}"))
        }
        e => super::upstream(e),
    })?;
    if verified.status != "active" {
        return Err(AppError::bad_request(format!(
            "this token is {}; create a new one",
            verified.status
        )));
    }
    let zones = client.zones().await.map_err(|e| {
        if e.is_forbidden() {
            AppError::bad_request(
                "the token can't list zones: add the Zone · Zone · Read permission",
            )
        } else {
            super::upstream(e)
        }
    })?;
    // Zone-scoped tokens may not list accounts; their zones still name one.
    let mut accounts = client.accounts().await.unwrap_or_default();
    for z in &zones {
        if let Some(a) = &z.account
            && !accounts.iter().any(|x| x.id == a.id)
        {
            accounts.push(a.clone());
        }
    }
    let account = match wanted.map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => Some(
            accounts
                .iter()
                .find(|a| a.id == id)
                .cloned()
                .ok_or_else(|| AppError::bad_request("the token can't access that account"))?,
        ),
        None if accounts.len() == 1 => accounts.first().cloned(),
        None => None,
    };
    Ok(Checked {
        token_id: verified.id,
        accounts,
        account,
        zones,
    })
}

/// Step 3 of the connect dialog: show what the token reaches. Stores nothing.
pub async fn verify(
    State(state): State<AppState>,
    Json(req): Json<TokenRequest>,
) -> AppResult<Json<Preview>> {
    let client = Client::new(
        state.cfg.cloudflare_api.clone(),
        super::parse_token(&req.token)?,
    );
    let c = check_token(&client, req.account_id.as_deref()).await?;
    Ok(Json(Preview {
        account_id: c.account.map(|a| a.id),
        accounts: c.accounts,
        zones: c.zones.into_iter().map(Into::into).collect(),
    }))
}

/// Connects (or replaces the token of) the workspace's Cloudflare integration.
pub async fn connect(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<TokenRequest>,
) -> AppResult<Json<Status>> {
    let workspace = grant.id();
    let token = super::parse_token(&req.token)?;
    let client = Client::new(state.cfg.cloudflare_api.clone(), token.clone());
    let c = check_token(&client, req.account_id.as_deref()).await?;
    if c.account.is_none() && c.accounts.len() > 1 {
        return Err(AppError::bad_request(
            "choose which Cloudflare account to use",
        ));
    }
    let metadata = Metadata {
        account_id: c.account.as_ref().map(|a| a.id.clone()),
        account_name: c.account.as_ref().map(|a| a.name.clone()),
        token_last4: super::last4(token.expose_secret()),
        token_id: c.token_id,
    };

    let mut tx = state.db.begin().await?;
    let existing: Option<Id> = sqlx::query_scalar(
        "SELECT id FROM integrations WHERE workspace_id = $1 AND kind = 'cloudflare' FOR UPDATE",
    )
    .bind(workspace)
    .fetch_optional(&mut *tx)
    .await?;
    // The AAD binds the row id, so a replaced token keeps the row.
    let id = existing.unwrap_or_default();
    let row = id.to_string();
    let sealed = state
        .keys
        .seal(
            &state.db,
            token.expose_secret().as_bytes(),
            super::secret_aad(workspace, &row),
        )
        .await?;
    let stored = sqlx::query(
        "INSERT INTO integrations (id, workspace_id, kind, secret_sealed, metadata, created_by)
         VALUES ($1, $2, 'cloudflare', $3, $4, $5)
         ON CONFLICT (workspace_id, kind) DO UPDATE
           SET secret_sealed = EXCLUDED.secret_sealed, metadata = EXCLUDED.metadata,
               created_by = EXCLUDED.created_by, updated_at = now()
           WHERE integrations.id = EXCLUDED.id",
    )
    .bind(id)
    .bind(workspace)
    .bind(&sealed)
    .bind(serde_json::to_value(&metadata).map_err(anyhow::Error::from)?)
    .bind(actor.user_id)
    .execute(&mut *tx)
    .await?;
    if stored.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "Cloudflare was connected at the same time; reload and try again".into(),
        ));
    }
    Entry::new(
        if existing.is_some() {
            "integration.update"
        } else {
            "integration.connect"
        },
        &meta,
    )
    .actor(&actor)
    .workspace(workspace)
    .target("integration", id)
    .metadata(json!({
        "kind": super::KIND,
        "accountId": metadata.account_id,
        "tokenLast4": metadata.token_last4,
        "zones": c.zones.len(),
    }))
    .record(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(load_status(&state, workspace).await?))
}

/// Forgets the token. DNS records already written stay at Cloudflare.
pub async fn disconnect(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<Status>> {
    let workspace = grant.id();
    let mut tx = state.db.begin().await?;
    let id: Option<Id> = sqlx::query_scalar(
        "DELETE FROM integrations WHERE workspace_id = $1 AND kind = 'cloudflare' RETURNING id",
    )
    .bind(workspace)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(id) = id else {
        return Err(AppError::NotFound);
    };
    Entry::new("integration.disconnect", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("integration", id)
        .metadata(json!({ "kind": super::KIND }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(load_status(&state, workspace).await?))
}

async fn require(state: &AppState, workspace: Id) -> AppResult<super::Connection> {
    super::connection(state, workspace)
        .await?
        .ok_or_else(|| AppError::Conflict("Cloudflare is not connected to this workspace".into()))
}

/// The zones the token can manage, live from Cloudflare.
pub async fn zones(
    State(state): State<AppState>,
    grant: Grant,
) -> AppResult<Json<Vec<ZoneSummary>>> {
    let conn = require(&state, grant.id()).await?;
    let zones = conn.client.zones().await.map_err(super::upstream)?;
    Ok(Json(zones.into_iter().map(Into::into).collect()))
}

// ---------------------------------------------------------------- R2

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct R2Info {
    pub account_id: String,
    pub endpoint: String,
    /// Whether the token may create buckets.
    pub can_create_buckets: bool,
    /// Where the user creates the S3 access key pair (the API can't).
    pub api_tokens_url: String,
}

pub async fn r2_info(State(state): State<AppState>, grant: Grant) -> AppResult<Json<R2Info>> {
    let conn = require(&state, grant.id()).await?;
    let account = conn.metadata.account_id.clone().ok_or_else(|| {
        AppError::Conflict("the Cloudflare token can't see an account; reconnect with one".into())
    })?;
    let can = conn
        .client
        .r2_access(&account)
        .await
        .map_err(super::upstream)?;
    Ok(Json(R2Info {
        endpoint: super::r2_endpoint(&account),
        api_tokens_url: format!("https://dash.cloudflare.com/{account}/r2/api-tokens"),
        account_id: account,
        can_create_buckets: can,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewR2Target {
    pub name: String,
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

/// "Use Cloudflare R2": creates the bucket through the API (if missing), then
/// adds an ordinary backup target for it through the backups code path.
pub async fn create_r2_target(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<NewR2Target>,
) -> AppResult<Json<backups::Target>> {
    let conn = require(&state, grant.id()).await?;
    let account = conn.metadata.account_id.clone().ok_or_else(|| {
        AppError::Conflict("the Cloudflare token can't see an account; reconnect with one".into())
    })?;
    let bucket = crate::backup::s3::bucket(&req.bucket)?;
    match conn.client.create_r2_bucket(&account, &bucket).await {
        Ok(_) => {}
        Err(e) if e.is_forbidden() => {
            return Err(AppError::bad_request(
                "the Cloudflare token can't create R2 buckets: add Account · Workers R2 Storage · Edit, or create the bucket in the dashboard and add it as S3 storage",
            ));
        }
        Err(e) => return Err(super::upstream(e)),
    }
    backups::create_target(
        State(state),
        actor,
        grant,
        meta,
        Json(backups::NewTarget {
            name: req.name,
            provider: "r2".into(),
            endpoint: super::r2_endpoint(&account),
            region: "auto".into(),
            bucket,
            prefix: req.prefix,
            access_key_id: req.access_key_id,
            secret_access_key: req.secret_access_key,
        }),
    )
    .await
}

// ---------------------------------------------------------------- domains

/// How a custom domain reaches the edge when Cloudflare manages its zone.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Ingress {
    /// Cloudflare is connected and the domain's zone is in it (DNS records
    /// are then written automatically).
    pub managed: bool,
    pub zone: Option<String>,
    /// "Proxied (orange cloud)" vs "DNS only".
    pub proxied: bool,
    /// The zone's SSL/TLS mode (`off`, `flexible`, `full`, `strict`), when
    /// the token may read zone settings.
    pub ssl_mode: Option<String>,
    /// Set when proxying would break or weaken TLS with the zone's SSL mode.
    pub ssl_warning: Option<String>,
    /// Set when Cloudflare could not be asked or updated.
    pub note: Option<String>,
}

#[derive(sqlx::FromRow)]
struct DomainRow {
    service_instance_id: Id,
    hostname: String,
    kind: String,
    verification_token: Option<String>,
    proxied: bool,
}

async fn domain_row(state: &AppState, id: Id) -> AppResult<DomainRow> {
    sqlx::query_as(
        "SELECT service_instance_id, hostname, kind, verification_token, proxied
         FROM domains WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

fn ssl_warning(mode: &str) -> Option<String> {
    match mode {
        "strict" => None,
        "full" => Some(
            "SSL mode is Full: Cloudflare doesn't validate this server's certificate. Use Full (strict).".into(),
        ),
        "flexible" => Some(
            "SSL mode is Flexible: Cloudflare talks to this server over plain HTTP, and HTTPS redirects loop. Set it to Full (strict).".into(),
        ),
        "off" => Some("SSL is off for this zone. Set it to Full (strict).".into()),
        other => Some(format!("Unknown SSL mode {other:?}; Full (strict) is required.")),
    }
}

/// Looks up the zone and its SSL mode; `None` zone = not managed.
async fn inspect(
    conn: &super::Connection,
    hostname: &str,
) -> (Option<Zone>, Option<String>, Option<String>) {
    let zone = match conn.client.find_zone(hostname).await {
        Ok(z) => z,
        Err(e) => return (None, None, Some(format!("Cloudflare: {e}"))),
    };
    let Some(zone) = zone else {
        return (None, None, None);
    };
    let ssl = match conn.client.ssl_mode(&zone.id).await {
        Ok(m) => Some(m),
        // Without Zone Settings: Read the mode is unknown, not an error.
        Err(e) if e.is_forbidden() => None,
        Err(e) => return (Some(zone), None, Some(format!("Cloudflare: {e}"))),
    };
    (Some(zone), ssl, None)
}

fn to_ingress(
    proxied: bool,
    zone: Option<Zone>,
    ssl: Option<String>,
    note: Option<String>,
) -> Ingress {
    let ssl_warning = match (&zone, proxied, ssl.as_deref()) {
        (Some(_), true, Some(m)) => ssl_warning(m),
        (Some(_), true, None) => Some(
            "Make sure the zone's SSL/TLS mode is Full (strict); the token can't read it (add Zone Settings: Read).".into(),
        ),
        _ => None,
    };
    Ingress {
        managed: zone.is_some(),
        zone: zone.map(|z| z.name),
        proxied,
        ssl_mode: ssl,
        ssl_warning,
        note,
    }
}

pub async fn ingress(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Ingress>> {
    let d = domain_row(&state, grant.id()).await?;
    let Some(conn) = super::connection(&state, grant.workspace_id()?).await? else {
        return Ok(Json(to_ingress(d.proxied, None, None, None)));
    };
    if d.kind != "custom" {
        return Ok(Json(to_ingress(d.proxied, None, None, None)));
    }
    let (zone, ssl, note) = inspect(&conn, &d.hostname).await;
    Ok(Json(to_ingress(d.proxied, zone, ssl, note)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SetIngress {
    pub proxied: bool,
}

/// Switches a custom domain between "DNS only" and "Proxied" and rewrites its
/// routing records (CNAME/A) at Cloudflare. The TXT proof is never proxied.
pub async fn set_ingress(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<SetIngress>,
) -> AppResult<Json<Ingress>> {
    let id = grant.id();
    let workspace = grant.workspace_id()?;
    let d = domain_row(&state, id).await?;
    if d.kind != "custom" {
        return Err(AppError::bad_request(
            "generated domains are served by this instance's own DNS",
        ));
    }
    let conn = require(&state, workspace).await?;
    let (zone, ssl, note) = inspect(&conn, &d.hostname).await;
    let Some(zone) = zone else {
        return Err(match note {
            Some(n) => AppError::Unavailable(n),
            None => AppError::Conflict(format!(
                "{} is not in a zone the Cloudflare token can manage",
                d.hostname
            )),
        });
    };
    let target = crate::domains::target(&state.db, d.service_instance_id).await?;
    let records = crate::domains::required_records(
        &d.hostname,
        d.verification_token.as_deref().unwrap_or_default(),
        &target,
    );
    let routing: Vec<(&str, &str)> = records
        .iter()
        .filter(|r| r.kind != "TXT")
        .map(|r| (r.kind, r.value.as_str()))
        .collect();
    conn.client
        .set_routing(&zone.id, &d.hostname, &routing, req.proxied)
        .await
        .map_err(super::upstream)?;
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE domains SET proxied = $2 WHERE id = $1")
        .bind(id)
        .bind(req.proxied)
        .execute(&mut *tx)
        .await?;
    Entry::new("domain.ingress", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("domain", id)
        .metadata(json!({
            "hostname": d.hostname,
            "proxied": req.proxied,
            "provider": super::KIND,
            "zone": zone.name,
        }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(to_ingress(req.proxied, Some(zone), ssl, note)))
}
