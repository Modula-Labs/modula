//! Alerts: a workspace's rules ("CPU above 80% for 5 min", "a deploy
//! failed") and the channels they notify (Slack, Discord, a signed JSON
//! webhook). The product sends no email.
//!
//! Event rules fire from jobs queued in the transactions that record the
//! event (see [`enqueue_event`]); metric and offline rules are evaluated
//! every minute ([`eval`]). Per rule and subject, a cooldown keeps a
//! flapping service from flooding a channel, and a metric alert that was
//! sent gets a "resolved" message when its condition clears.

pub mod eval;
pub mod format;
pub mod send;

use modula_crypto::envelope::FieldAad;
use modula_id::Id;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::error::AppResult;
use crate::jobs::{self, NewJob};

pub const EVENT_JOB: &str = "alerts.event";
pub const EVALUATE_JOB: &str = "alerts.evaluate";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A deployment failed after its build (pulling, starting, health check,
    /// pre-deploy command).
    DeploymentFailed,
    BuildFailed,
    DeploymentCrashed,
    BackupFailed,
    /// CPU above `threshold` percent of the limit (or of the server's CPUs)
    /// for `window_minutes`.
    Cpu,
    /// Memory above `threshold` percent of the service's memory limit for
    /// `window_minutes` (services without a limit aren't evaluated).
    Memory,
    /// A server's disk (the filesystem of Modula's data or Docker's, where
    /// volumes live) more than `threshold` percent full.
    Disk,
    /// A server offline for `window_minutes`.
    NodeOffline,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeploymentFailed => "deployment_failed",
            Self::BuildFailed => "build_failed",
            Self::DeploymentCrashed => "deployment_crashed",
            Self::BackupFailed => "backup_failed",
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Disk => "disk",
            Self::NodeOffline => "node_offline",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        serde_json::from_value(json!(s)).ok()
    }

    /// Fired by an event rather than evaluated: never "resolved".
    pub fn is_event(self) -> bool {
        matches!(
            self,
            Self::DeploymentFailed
                | Self::BuildFailed
                | Self::DeploymentCrashed
                | Self::BackupFailed
        )
    }

    pub fn needs_threshold(self) -> bool {
        matches!(self, Self::Cpu | Self::Memory | Self::Disk)
    }

    /// The allowed `window_minutes`, for kinds that have one.
    pub fn window_range(self) -> Option<(i32, i32)> {
        match self {
            Self::Cpu | Self::Memory => Some((1, 60)),
            Self::NodeOffline => Some((1, 1440)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Fired,
    Resolved,
    Test,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fired => "fired",
            Self::Resolved => "resolved",
            Self::Test => "test",
        }
    }
}

/// One notification, whatever the channel.
#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub phase: Phase,
    pub rule_id: Option<Id>,
    pub rule_name: String,
    pub kind: Option<Kind>,
    /// "instance:<id>" or "node:<id>".
    pub subject: String,
    /// One line, e.g. "web failed to deploy".
    pub title: String,
    /// Why: the deployment's error, the measured value.
    pub reason: String,
    pub project: Option<String>,
    pub environment: Option<String>,
    pub service: Option<String>,
    pub server: Option<String>,
    /// Where to look in the dashboard.
    pub link: String,
    pub at: OffsetDateTime,
}

pub fn url_aad(workspace: Id, row: &str) -> FieldAad<'_> {
    FieldAad {
        scope: workspace,
        table: "notification_channels",
        column: "url",
        row,
    }
}

pub fn secret_aad(workspace: Id, row: &str) -> FieldAad<'_> {
    FieldAad {
        scope: workspace,
        table: "notification_channels",
        column: "secret",
        row,
    }
}

/// An event an event rule may fire on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub kind: Kind,
    pub service_instance_id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_id: Option<Id>,
}

/// Queues the evaluation of `event` against the workspace's rules (inside
/// the transaction that records the event; the job finds the rules).
pub async fn enqueue_event<'e>(db: impl sqlx::PgExecutor<'e>, event: Event) -> AppResult<()> {
    jobs::enqueue(
        db,
        NewJob {
            kind: EVENT_JOB,
            payload: serde_json::to_value(&event).map_err(anyhow::Error::from)?,
            max_attempts: Some(1),
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}

/// A deployment's failure is a build failure while it was building.
pub fn failure_kind(previous_status: &str) -> Kind {
    if previous_status == "building" {
        Kind::BuildFailed
    } else {
        Kind::DeploymentFailed
    }
}

/// Drop sends older than a month and disk samples older than a week.
pub async fn purge(db: &sqlx::PgPool) -> anyhow::Result<u64> {
    let a = sqlx::query("DELETE FROM alert_sends WHERE at < now() - interval '30 days'")
        .execute(db)
        .await?
        .rows_affected();
    let b = sqlx::query("DELETE FROM disk_samples WHERE at < now() - interval '7 days'")
        .execute(db)
        .await?
        .rows_affected();
    Ok(a + b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip() {
        for k in [
            Kind::DeploymentFailed,
            Kind::BuildFailed,
            Kind::DeploymentCrashed,
            Kind::BackupFailed,
            Kind::Cpu,
            Kind::Memory,
            Kind::Disk,
            Kind::NodeOffline,
        ] {
            assert_eq!(Kind::parse(k.as_str()), Some(k));
        }
        assert_eq!(Kind::parse("email"), None);
        assert_eq!(failure_kind("building"), Kind::BuildFailed);
        assert_eq!(failure_kind("deploying"), Kind::DeploymentFailed);
    }
}
