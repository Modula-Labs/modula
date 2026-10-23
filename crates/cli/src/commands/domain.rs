//! `domain`: generate one, or add, list, check and delete domains.

use crate::api::{Api, Domain};
use crate::cli::{DomainArgs, DomainCommand};
use crate::context::Ctx;
use crate::output::{Output, Table, or_dash};

fn records_table(d: &Domain) -> Table {
    let mut t = Table::new(&["type", "name", "value"]);
    for r in &d.dns_records {
        t.row([r.kind.as_str(), &r.name, &r.value]);
    }
    t
}

fn domains_table(domains: &[Domain]) -> Table {
    let mut t = Table::new(&["hostname", "kind", "status", "port", "error"]).status_column(2);
    for d in domains {
        t.row([
            d.hostname.clone(),
            d.kind.clone(),
            d.status.clone(),
            d.target_port.map_or_else(|| "-".into(), |p| p.to_string()),
            or_dash(d.last_error.as_deref()),
        ]);
    }
    t
}

/// What to create at the DNS host, and what happens next.
fn print_custom(o: &Output, d: &Domain) {
    o.line(format!("{} is {}", d.hostname, o.status(&d.status)));
    if d.status != "verified" && !d.dns_records.is_empty() {
        o.line("Create these DNS records:");
        o.table(&records_table(d));
        o.note(format!(
            "Then run `modula-deploy domain check {}` until it is verified.",
            d.hostname
        ));
    }
    if let Some(note) = &d.provider_note {
        o.note(note);
    }
}

async fn find(api: &Api, instance: &str, host: &str) -> anyhow::Result<Domain> {
    let domains = api.domains(instance).await?;
    let host = host.trim().trim_end_matches('.');
    domains
        .into_iter()
        .find(|d| d.hostname.eq_ignore_ascii_case(host))
        .ok_or_else(|| anyhow::anyhow!("{host} isn't a domain of this service"))
}

pub async fn run(ctx: &Ctx, args: DomainArgs) -> anyhow::Result<()> {
    let t = ctx.target().await?;
    let api = ctx.api()?;
    match args.command {
        None => {
            let mut d = api.generate_domain(&t.instance).await?;
            if let Some(port) = args.port {
                d = api.update_domain(&d.id, Some(port), None).await?;
            }
            ctx.out
                .either(&d, |o| o.line(format!("https://{}", d.hostname)))
        }
        Some(DomainCommand::Add { host, port }) => {
            let d = api.add_custom_domain(&t.instance, &host, port).await?;
            ctx.out.either(&d, |o| print_custom(o, &d))
        }
        Some(DomainCommand::List) => {
            let domains = api.domains(&t.instance).await?;
            ctx.out.either(&domains, |o| {
                if domains.is_empty() {
                    o.line(format!("{} has no domains.", t.label()));
                } else {
                    o.table(&domains_table(&domains));
                }
            })
        }
        Some(DomainCommand::Check { host }) => {
            let d = find(api, &t.instance, &host).await?;
            let d = api.check_domain(&d.id).await?;
            ctx.out.either(&d, |o| {
                print_custom(o, &d);
                if let Some(e) = &d.last_error {
                    o.line(format!("Last error: {e}"));
                }
            })
        }
        Some(DomainCommand::Delete { host }) => {
            let d = find(api, &t.instance, &host).await?;
            ctx.confirm(&format!("Remove {} from {}?", d.hostname, t.label()))?;
            let r = api.delete_domain(&d.id).await?;
            ctx.out.either(&r, |o| {
                o.line(format!("Removed {}", d.hostname));
                if let Some(note) = r["providerNote"].as_str() {
                    o.note(note);
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_records_render_as_a_table() {
        let d: Domain = serde_json::from_value(serde_json::json!({
            "id": "d1", "hostname": "api.example.com", "kind": "custom", "status": "pending",
            "dnsRecords": [
                { "type": "CNAME", "name": "api.example.com", "value": "api-x1y2.apps.example.net" },
                { "type": "TXT", "name": "_modula-challenge.api.example.com", "value": "tok" }
            ],
            "rateLimitEnforced": false
        }))
        .unwrap();
        assert_eq!(
            records_table(&d).render(false),
            "TYPE   NAME                               VALUE\n\
             CNAME  api.example.com                    api-x1y2.apps.example.net\n\
             TXT    _modula-challenge.api.example.com  tok\n"
        );
        let list = domains_table(&[d]).render(false);
        assert!(list.starts_with("HOSTNAME"), "{list}");
        assert!(list.contains("custom  pending  -     -"), "{list}");
    }
}
