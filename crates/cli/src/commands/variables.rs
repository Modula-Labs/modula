//! `variables`: list names, set and delete. Values are write-only over the
//! API (revealing one needs a re-authenticated dashboard session, which
//! tokens never have), so listings show names and metadata only.

use std::collections::BTreeMap;
use std::io::Read;

use crate::api::Environment;
use crate::api::VariableChanges;
use crate::api::endpoints::VariableScope;
use crate::cli::{VariablesArgs, VariablesCommand};
use crate::commands::deploy;
use crate::context::Ctx;
use crate::output::{Table, short_time, usage};

const MASK: &str = "********";

/// `KEY=VALUE` (split at the first `=`; the value may be empty or contain `=`).
pub fn parse_pair(raw: &str) -> anyhow::Result<(String, String)> {
    let Some((key, value)) = raw.split_once('=') else {
        return Err(usage(format!("expected KEY=VALUE, got {raw:?}")));
    };
    let key = key.trim();
    if key.is_empty() {
        return Err(usage(format!("missing the name in {raw:?}")));
    }
    Ok((key.to_owned(), value.to_owned()))
}

pub fn parse_pairs(raw: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for r in raw {
        let (k, v) = parse_pair(r)?;
        if out.insert(k.clone(), v).is_some() {
            return Err(usage(format!("{k} is given twice")));
        }
    }
    Ok(out)
}

/// Dotenv-style lines: `KEY=VALUE`, `export KEY=VALUE`, blank lines and
/// `#` comments; one level of matching quotes around the value is removed.
pub fn parse_dotenv(text: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (k, v) = parse_pair(line)?;
        let v = v.trim();
        let v = ['"', '\'']
            .iter()
            .find_map(|q| {
                v.strip_prefix(*q)
                    .and_then(|rest| rest.strip_suffix(*q))
                    .filter(|_| v.len() >= 2)
            })
            .unwrap_or(v);
        out.insert(k, v.to_owned());
    }
    Ok(out)
}

/// Whose variables: a service instance, or (`--shared`) an environment.
struct Place {
    environment: Environment,
    instance: Option<String>,
    service_id: Option<String>,
    label: String,
}

impl Place {
    async fn resolve(ctx: &Ctx, shared: bool) -> anyhow::Result<Self> {
        if shared {
            let p = ctx.project().await?;
            let environment = ctx.environment(&p)?.clone();
            return Ok(Self {
                label: format!("shared variables of {}/{}", p.name, environment.name),
                environment,
                instance: None,
                service_id: None,
            });
        }
        let t = ctx.target().await?;
        Ok(Self {
            label: t.label(),
            environment: t.environment,
            instance: Some(t.instance),
            service_id: Some(t.service.id),
        })
    }

    fn scope(&self) -> VariableScope<'_> {
        match &self.instance {
            Some(i) => VariableScope::Instance(i),
            None => VariableScope::Shared(&self.environment.id),
        }
    }
}

pub async fn run(ctx: &Ctx, args: VariablesArgs) -> anyhow::Result<i32> {
    let place = Place::resolve(ctx, args.shared).await?;
    let (scope, label) = (place.scope(), &place.label);
    let api = ctx.api()?;
    match args.command {
        None => {
            let vars = api.variables(scope).await?;
            if args.kv && !ctx.out.json {
                for v in &vars {
                    ctx.out.line(format!("{}={MASK}", v.name));
                }
                ctx.out
                    .note("Values are write-only over the API; reveal them in the dashboard.");
                return Ok(0);
            }
            ctx.out.either(&vars, |o| {
                if vars.is_empty() {
                    o.line(format!("No variables in {label}."));
                    return;
                }
                let mut t = Table::new(&["name", "value", "references", "updated"]);
                for v in &vars {
                    t.row([
                        v.name.clone(),
                        if v.sealed {
                            "sealed".into()
                        } else {
                            MASK.into()
                        },
                        v.references.join(", "),
                        short_time(&v.updated_at),
                    ]);
                }
                o.table(&t);
            })?;
        }
        Some(VariablesCommand::Set {
            pairs,
            stdin,
            sealed,
            deploy: deploy_now,
            skip_deploys: _,
        }) => {
            let mut set = parse_pairs(&pairs)?;
            if stdin {
                let mut text = String::new();
                std::io::stdin().read_to_string(&mut text)?;
                for (k, v) in parse_dotenv(&text)? {
                    set.insert(k, v);
                }
            }
            if set.is_empty() {
                return Err(usage("nothing to set"));
            }
            if sealed {
                ctx.confirm(&format!(
                    "Seal {} in {label}? Sealed values can never be revealed or copied.",
                    set.keys().cloned().collect::<Vec<_>>().join(", ")
                ))?;
            }
            let names: Vec<String> = set.keys().cloned().collect();
            let changes = VariableChanges {
                sealed: if sealed { names.clone() } else { Vec::new() },
                set,
                delete: Vec::new(),
            };
            let vars = api.update_variables(scope, &changes).await?;
            // With --deploy --json the deploy result is the one document.
            if !(ctx.out.json && deploy_now) {
                ctx.out.either(&vars, |o| {
                    o.line(format!("Set {} in {label}", names.join(", ")));
                })?;
            }
            return after_change(ctx, &place, deploy_now).await;
        }
        Some(VariablesCommand::Delete {
            keys,
            deploy: deploy_now,
        }) => {
            ctx.confirm(&format!("Delete {} from {label}?", keys.join(", ")))?;
            let changes = VariableChanges {
                delete: keys.clone(),
                ..Default::default()
            };
            let vars = api.update_variables(scope, &changes).await?;
            // With --deploy --json the deploy result is the one document.
            if !(ctx.out.json && deploy_now) {
                ctx.out.either(&vars, |o| {
                    o.line(format!("Deleted {} from {label}", keys.join(", ")));
                })?;
            }
            return after_change(ctx, &place, deploy_now).await;
        }
    }
    Ok(0)
}

/// Changes stay pending until deployed; `--deploy` does that now.
async fn after_change(ctx: &Ctx, place: &Place, deploy_now: bool) -> anyhow::Result<i32> {
    if !deploy_now {
        ctx.out
            .note("Pending until deployed: run `modula-deploy deploy` (or pass --deploy).");
        return Ok(0);
    }
    // A shared variable's change deploys every service it affects.
    let services = place.service_id.clone().map(|s| vec![s]);
    deploy::deploy_environment(ctx, &place.environment, services, false).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_split_at_the_first_equals() {
        assert_eq!(
            parse_pair("DATABASE_URL=postgres://u:p@h/db?x=1").unwrap(),
            ("DATABASE_URL".into(), "postgres://u:p@h/db?x=1".into())
        );
        assert_eq!(parse_pair("EMPTY=").unwrap(), ("EMPTY".into(), "".into()));
        let e = parse_pair("NOVALUE").unwrap_err();
        assert!(e.downcast_ref::<crate::output::UsageError>().is_some());
        assert!(parse_pair("=x").is_err());
        let m = parse_pairs(&["A=1".into(), "B=${{postgres.DATABASE_URL}}".into()]).unwrap();
        assert_eq!(m["B"], "${{postgres.DATABASE_URL}}");
        assert!(parse_pairs(&["A=1".into(), "A=2".into()]).is_err());
    }

    #[test]
    fn dotenv_lines() {
        let m =
            parse_dotenv("# comment\n\nexport A=1\nB=\"two words\"\nC='x=y'\nD=\"\nE=\n").unwrap();
        assert_eq!(m["A"], "1");
        assert_eq!(m["B"], "two words");
        assert_eq!(m["C"], "x=y");
        assert_eq!(m["D"], "\"");
        assert_eq!(m["E"], "");
        assert!(parse_dotenv("oops").is_err());
    }
}
