//! Cron services: the schedule's next due times, the runs, "Run now", and a
//! preview of an expression as it is typed (see `crate::cron`).

use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::cron::{self, Run, Trigger};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;

const RUNS_SHOWN: i64 = 50;
const UPCOMING: usize = 3;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub expression: String,
    /// For humans, e.g. "every day at 03:00 UTC".
    pub description: String,
    /// The next due times (UTC), soonest first.
    #[serde(with = "rfc3339_list")]
    pub upcoming: Vec<OffsetDateTime>,
    /// False while the schedule is only in the settings (not deployed yet).
    pub deployed: bool,
    pub timeout_secs: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CronRuns {
    /// None: not a cron service.
    pub schedule: Option<Schedule>,
    /// Newest first.
    pub runs: Vec<Run>,
}

mod rfc3339_list {
    use serde::Serializer;
    use serde::ser::SerializeSeq;
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    pub fn serialize<S: Serializer>(v: &[OffsetDateTime], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(v.len()))?;
        for t in v {
            seq.serialize_element(&t.format(&Rfc3339).map_err(serde::ser::Error::custom)?)?;
        }
        seq.end()
    }
}

pub async fn list(State(state): State<AppState>, grant: Grant) -> AppResult<Json<CronRuns>> {
    let instance = grant.id();
    // The deployed schedule if there is one, else the configured one.
    let row: Option<(Option<serde_json::Value>, serde_json::Value)> = sqlx::query_as(
        "SELECT (SELECT d.config FROM deployments d
                 WHERE d.service_instance_id = i.id AND d.status = 'active'
                 ORDER BY d.created_at DESC LIMIT 1),
                i.config
         FROM service_instances i WHERE i.id = $1",
    )
    .bind(instance)
    .fetch_optional(&state.db)
    .await?;
    let Some((deployed_cfg, instance_cfg)) = row else {
        return Err(AppError::NotFound);
    };
    let read = |v: &serde_json::Value| {
        serde_json::from_value::<crate::deploy::config::ServiceConfig>(v.clone()).ok()
    };
    let deployed = deployed_cfg
        .as_ref()
        .and_then(read)
        .filter(|c| c.cron_schedule.is_some());
    let configured = read(&instance_cfg).filter(|c| c.cron_schedule.is_some());
    let schedule = match (deployed, configured) {
        (Some(d), _) => {
            let expr = d.cron_schedule.clone().unwrap_or_default();
            let now = OffsetDateTime::now_utc();
            // From the stored next due time, so a claimed one isn't shown again.
            let next = cron::next_run_at(&state, instance).await?;
            let upcoming = match (cron::parse(&expr), next) {
                (Ok(c), Some(n)) => std::iter::once(n)
                    .chain(cron::upcoming(&c, n, UPCOMING - 1))
                    .collect(),
                (Ok(c), None) => cron::upcoming(&c, now, UPCOMING),
                (Err(_), _) => Vec::new(),
            };
            Some(Schedule {
                description: cron::describe(&expr),
                expression: expr,
                upcoming,
                deployed: true,
                timeout_secs: d.cron_timeout(),
            })
        }
        (None, Some(c)) => {
            let expr = c.cron_schedule.clone().unwrap_or_default();
            Some(Schedule {
                description: cron::describe(&expr),
                upcoming: cron::parse(&expr)
                    .map(|p| cron::upcoming(&p, OffsetDateTime::now_utc(), UPCOMING))
                    .unwrap_or_default(),
                expression: expr,
                deployed: false,
                timeout_secs: c.cron_timeout(),
            })
        }
        (None, None) => None,
    };
    Ok(Json(CronRuns {
        schedule,
        runs: cron::list(&state, instance, RUNS_SHOWN).await?,
    }))
}

/// Run now, from the serving deployment (whatever the schedule says).
pub async fn run_now(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<Run>> {
    let instance = grant.id();
    let id = cron::start(&state, instance, Trigger::Manual, None, Some(actor.user_id)).await?;
    Entry::new("cron.run", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("cron_run", id)
        .metadata(json!({ "serviceInstance": instance }))
        .record(&state.db)
        .await?;
    cron::load(&state, id).await.map(Json)
}

#[derive(Deserialize)]
pub struct PreviewQuery {
    pub schedule: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub valid: bool,
    pub error: Option<String>,
    pub description: Option<String>,
    #[serde(with = "rfc3339_list")]
    pub upcoming: Vec<OffsetDateTime>,
}

/// What an expression means and when it would run next (the settings form).
pub async fn preview(Query(q): Query<PreviewQuery>) -> Json<Preview> {
    Json(match cron::parse(&q.schedule) {
        Ok(c) => Preview {
            valid: true,
            error: None,
            description: Some(cron::describe(&q.schedule)),
            upcoming: cron::upcoming(&c, OffsetDateTime::now_utc(), UPCOMING),
        },
        Err(e) => Preview {
            valid: false,
            error: Some(e),
            description: None,
            upcoming: Vec::new(),
        },
    })
}
