//! `db query`, `db tables` (Postgres, run by the server's node through the
//! API) and `connect` (a private tunnel and the native client).

use serde_json::Value;

use crate::api::{QueryResult, Tables};
use crate::cli::DbCommand;
use crate::connect::{self, ConnectArgs};
use crate::context::Ctx;
use crate::output::Table;

fn cell(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A result set as a table; rows may be arrays (the API's shape) or objects.
pub fn query_table(r: &QueryResult) -> Table {
    let names: Vec<&str> = r.columns.iter().map(|c| c.name.as_str()).collect();
    let mut t = Table::verbatim(&names);
    for row in &r.rows {
        let cells: Vec<String> = match row {
            Value::Array(cells) => cells.iter().map(cell).collect(),
            Value::Object(map) => names
                .iter()
                .map(|n| map.get(*n).map_or_else(String::new, cell))
                .collect(),
            other => vec![cell(other)],
        };
        t.row(cells);
    }
    t
}

/// `3 rows (SELECT, 4 ms)`, `UPDATE 2 (12 ms)`.
pub fn summary(r: &QueryResult) -> String {
    let count = r.row_count.unwrap_or(r.rows.len() as u64);
    let command = r.command.clone().unwrap_or_default();
    let ms = r
        .duration_ms
        .map(|ms| format!("{} ms", ms.round()))
        .unwrap_or_default();
    let mut text = if r.columns.is_empty() {
        format!("{command} {count}").trim().to_owned()
    } else {
        format!("{count} row{}", if count == 1 { "" } else { "s" })
    };
    let detail: Vec<&str> = [if r.columns.is_empty() { "" } else { &command }, &ms]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    if !detail.is_empty() {
        text.push_str(&format!(" ({})", detail.join(", ")));
    }
    if r.truncated {
        text.push_str(", truncated");
    }
    text
}

pub async fn run(ctx: &Ctx, command: DbCommand) -> anyhow::Result<()> {
    let t = ctx.database_target().await?;
    let api = ctx.api()?;
    match command {
        DbCommand::Query { sql, write } => {
            if write {
                ctx.confirm(&format!("Run this SQL with write access on {}?", t.label()))?;
            }
            let raw = api.db_query(&t.instance, &sql, !write).await?;
            let r: QueryResult = serde_json::from_value(raw.clone()).unwrap_or_default();
            ctx.out.either(&raw, |o| {
                if !r.columns.is_empty() {
                    o.table(&query_table(&r));
                }
                o.line(summary(&r));
            })
        }
        DbCommand::Tables => {
            let raw = api.db_tables(&t.instance).await?;
            let tables: Tables = serde_json::from_value(raw.clone()).unwrap_or_default();
            ctx.out.either(&raw, |o| {
                if tables.tables.is_empty() {
                    o.line("No tables.");
                    return;
                }
                let mut tb = Table::new(&["schema", "table", "rows (estimate)"]);
                for x in &tables.tables {
                    tb.row([
                        x.schema.clone(),
                        x.name.clone(),
                        x.row_estimate.map_or_else(|| "-".into(), |n| n.to_string()),
                    ]);
                }
                o.table(&tb);
            })
        }
    }
}

pub async fn connect(
    ctx: &Ctx,
    service: Option<String>,
    tunnel_only: bool,
    show_password: bool,
    port: Option<u16>,
) -> anyhow::Result<i32> {
    let api = ctx.api()?;
    // The positional service wins; then -s; then the linked database.
    let service = match service.or_else(|| ctx.global.service.clone()) {
        Some(s) => Some(s),
        None => {
            let p = ctx.project().await?;
            ctx.database_service(&p).ok().map(|s| s.id.clone())
        }
    };
    connect::run(
        api,
        ctx.linked(),
        ConnectArgs {
            service,
            environment: ctx.global.environment.clone(),
            project: ctx.global.project.clone(),
            tunnel_only,
            port,
            show_password,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn result_sets_render() {
        let r: QueryResult = serde_json::from_value(json!({
            "columns": [{ "name": "id", "type": "int4" }, { "name": "email", "type": "text" }],
            "rows": [["1", "a@example.com"], ["2", null]],
            "rowCount": 2, "command": "SELECT", "durationMs": 3.6, "truncated": false
        }))
        .unwrap();
        assert_eq!(
            query_table(&r).render(false),
            "id  email\n1   a@example.com\n2   NULL\n"
        );
        assert_eq!(summary(&r), "2 rows (SELECT, 4 ms)");

        let objects: QueryResult = serde_json::from_value(json!({
            "columns": [{ "name": "n" }], "rows": [{ "n": 5 }]
        }))
        .unwrap();
        assert_eq!(query_table(&objects).render(false), "n\n5\n");
        assert_eq!(summary(&objects), "1 row");

        let update: QueryResult =
            serde_json::from_value(json!({ "rowCount": 3, "command": "UPDATE", "durationMs": 12 }))
                .unwrap();
        assert_eq!(summary(&update), "UPDATE 3 (12 ms)");
    }
}
