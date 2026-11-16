//! A workspace's spending guards per provider, in USD.

use modula_id::Id;
use modula_provider::eur_cents_to_usd;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgExecutor;

use crate::error::AppResult;

/// Spending guards, stored in `integrations.metadata`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Caps {
    pub max_servers: i64,
    /// USD cents per month, across all servers created here.
    pub max_monthly_cents: i64,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            max_servers: 5,
            max_monthly_cents: 10_000,
        }
    }
}

impl Caps {
    pub const MAX_SERVERS: i64 = 100;
    pub const MAX_MONTHLY_CENTS: i64 = 10_000_000;

    /// Caps as stored. Metadata not marked by [`Self::to_metadata`] predates
    /// USD: its limit is in euro cents and converted at `eur_usd_rate`.
    pub fn from_metadata(v: &serde_json::Value, eur_usd_rate: f64) -> Self {
        let d = Self::default();
        let usd = v["capsCurrency"].as_str() == Some("usd");
        Self {
            max_servers: v["maxServers"].as_i64().unwrap_or(d.max_servers),
            max_monthly_cents: match v["maxMonthlyCents"].as_i64() {
                Some(c) if usd => c,
                Some(c) => eur_cents_to_usd(c, eur_usd_rate),
                None => d.max_monthly_cents,
            },
        }
    }

    /// What to merge into `integrations.metadata`.
    pub fn to_metadata(self) -> serde_json::Value {
        json!({
            "maxServers": self.max_servers,
            "maxMonthlyCents": self.max_monthly_cents,
            "capsCurrency": "usd",
        })
    }
}

/// What the workspace has provisioned at a provider so far (counts every row
/// created here: a failed server may still exist, and bill, until it is
/// deleted; adopted servers were created elsewhere and count toward no caps).
#[derive(Debug, Clone, Copy, Serialize, Default, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub servers: i64,
    pub monthly_cents: i64,
}

pub async fn usage<'e>(db: impl PgExecutor<'e>, workspace: Id, kind: &str) -> AppResult<Usage> {
    Ok(sqlx::query_as(
        "SELECT count(*) AS servers, COALESCE(sum(monthly_price_cents), 0)::bigint AS monthly_cents
         FROM provisioned_servers
         WHERE workspace_id = $1 AND provider = $2 AND origin = 'provisioned'",
    )
    .bind(workspace)
    .bind(kind)
    .fetch_one(db)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_default_and_parse() {
        assert_eq!(
            Caps::from_metadata(&json!({}), 1.08),
            Caps::default(),
            "the default limit is already USD"
        );
        let c = Caps {
            max_servers: 2,
            max_monthly_cents: 999,
        };
        assert_eq!(Caps::from_metadata(&c.to_metadata(), 1.08), c);
    }

    #[test]
    fn limits_stored_before_usd_are_converted_when_read() {
        let c = Caps::from_metadata(&json!({"maxServers": 2, "maxMonthlyCents": 1000}), 1.08);
        assert_eq!((c.max_servers, c.max_monthly_cents), (2, 1080));
        // Saving pins it in USD, whatever the rate does afterwards.
        assert_eq!(Caps::from_metadata(&c.to_metadata(), 1.5), c);
    }
}
