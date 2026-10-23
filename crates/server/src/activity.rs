//! The project's activity feed: configuration changes (recorded here, one
//! change set per request) interleaved with deployments.
//!
//! Only names are recorded — variable names and config fields — never values.

use std::collections::BTreeSet;

use modula_id::Id;
use serde_json::Value;

use crate::error::AppResult;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// `variable`, `config` or `service`.
    pub kind: &'static str,
    /// `added`, `updated`, `removed`, `created` or `deleted`.
    pub op: &'static str,
    pub key: Option<String>,
}

impl Change {
    pub fn new(kind: &'static str, op: &'static str, key: impl Into<String>) -> Self {
        Self {
            kind,
            op,
            key: Some(key.into()),
        }
    }
}

/// The top-level config fields that differ between two snapshots.
pub fn config_changes(old: &Value, new: &Value) -> Vec<Change> {
    let (Some(old), Some(new)) = (old.as_object(), new.as_object()) else {
        return vec![];
    };
    let keys: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    keys.into_iter()
        .filter_map(|k| {
            let op = match (old.get(k), new.get(k)) {
                (None, Some(_)) => "added",
                (Some(_), None) => "removed",
                (Some(a), Some(b)) if a != b => "updated",
                _ => return None,
            };
            Some(Change::new("config", op, k.as_str()))
        })
        .collect()
}

/// Record changes made by one request as an applied change set. Nothing is
/// written when there are no changes.
pub async fn record(
    tx: &mut sqlx::PgConnection,
    environment: Id,
    service: Option<Id>,
    user: Option<Id>,
    changes: &[Change],
) -> AppResult<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let set = Id::new();
    sqlx::query(
        "INSERT INTO change_sets (id, environment_id, status, created_by, applied_at, applied_by)
         VALUES ($1, $2, 'applied', $3, now(), $3)",
    )
    .bind(set)
    .bind(environment)
    .bind(user)
    .execute(&mut *tx)
    .await?;
    for c in changes {
        sqlx::query(
            "INSERT INTO changes (change_set_id, service_id, kind, op, key) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(set)
        .bind(service)
        .bind(c.kind)
        .bind(c.op)
        .bind(&c.key)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn config_changes_name_fields_not_values() {
        let old = json!({ "port": 8080, "startCommand": ["a"], "source": { "image": "x" } });
        let new = json!({ "port": 3000, "source": { "image": "x" }, "memoryMb": 512 });
        assert_eq!(
            config_changes(&old, &new),
            [
                Change::new("config", "added", "memoryMb"),
                Change::new("config", "updated", "port"),
                Change::new("config", "removed", "startCommand"),
            ]
        );
        assert!(config_changes(&new, &new).is_empty());
    }
}
