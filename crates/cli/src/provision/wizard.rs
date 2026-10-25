//! The interactive side of `server create`: one question per step, in
//! Modula's teal, with Esc going back a step (up to the final confirmation)
//! and Ctrl-C quitting. Answers given as flags are taken as-is and skipped.

use std::fmt;

use inquire::ui::{Attributes, Color, RenderConfig, StyleSheet, Styled};
use inquire::validator::Validation;
use inquire::{Confirm, InquireError, Password, PasswordDisplayMode, Select, Text};
use modula_hetzner::SshKey;
use secrecy::{ExposeSecret, SecretString};

use super::catalog::{self, LocationOption};
use super::flow::{self, Created, DnsPlan, Plan, Secrets, Settings, SshChoice};
use super::{
    DEFAULT_ADMIN_NAME, DEFAULT_NAME, EnvSecrets, cloud_init, cloudflare, cloudflare_lookup,
    describe_records, is_auto, key_body, load_catalog, local_public_keys, read_public_key,
    ssh_choice_for_file, valid_admin_name, valid_domain, valid_email, valid_password,
    valid_server_name,
};
use crate::cli::ServerCreateArgs;
use crate::context::Ctx;
use crate::output::Output;

const TEAL: Color = Color::Rgb {
    r: 45,
    g: 212,
    b: 191,
};

pub(crate) fn theme() -> RenderConfig<'static> {
    let teal = StyleSheet::new().with_fg(TEAL);
    RenderConfig::default_colored()
        .with_prompt_prefix(Styled::new("◆").with_fg(TEAL))
        .with_answered_prompt_prefix(Styled::new("✓").with_fg(TEAL))
        .with_highlighted_option_prefix(Styled::new("›").with_fg(TEAL))
        .with_selected_option(Some(teal.with_attr(Attributes::BOLD)))
        .with_answer(teal)
        .with_help_message(StyleSheet::new().with_fg(Color::DarkGrey))
}

/// Where a step leaves the wizard.
enum Nav {
    Next,
    Back,
}

/// Esc → back a step; Ctrl-C → quit.
fn answer<T>(r: Result<T, InquireError>) -> anyhow::Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(InquireError::OperationCanceled) => Ok(None),
        Err(InquireError::OperationInterrupted) => Err(Quit.into()),
        Err(e) => Err(e.into()),
    }
}

/// The user quit (Ctrl-C).
#[derive(Debug)]
struct Quit;

impl fmt::Display for Quit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Quit {}

/// A select option: what's shown, and what it stands for.
struct Choice<T> {
    label: String,
    value: T,
}

impl<T> fmt::Display for Choice<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Step {
    Token,
    Location,
    Type,
    Name,
    Domain,
    Dns,
    Email,
    Ssh,
    Confirm,
}

const STEPS: [Step; 9] = [
    Step::Token,
    Step::Location,
    Step::Type,
    Step::Name,
    Step::Domain,
    Step::Dns,
    Step::Email,
    Step::Ssh,
    Step::Confirm,
];

/// The answers so far.
#[derive(Default)]
struct Draft {
    hetzner: Option<SecretString>,
    catalog: Vec<LocationOption>,
    ssh_keys: Vec<SshKey>,
    location: Option<usize>,
    server_type: Option<String>,
    name: Option<String>,
    domain: Option<String>,
    dns: Option<DnsPlan>,
    cloudflare: Option<SecretString>,
    email: Option<String>,
    ssh: Option<SshChoice>,
}

fn say(out: &Output, text: &str) {
    out.note(format!("  {text}"));
}

/// Run the wizard. `None` when the user backed out of the first step.
pub async fn run(
    ctx: &Ctx,
    args: &ServerCreateArgs,
    env: &mut EnvSecrets,
    settings: &Settings,
) -> anyhow::Result<Option<(Plan, Secrets)>> {
    inquire::set_global_render_config(theme());
    let out = &ctx.out;
    out.note("");
    out.note(format!(
        "  {}  {}",
        out.accent("◆ modula"),
        out.dim("deploy · your own little cloud")
    ));
    say(
        out,
        "Let's create your Modula server on Hetzner Cloud. Esc goes back a step.",
    );
    out.note("");

    let mut d = Draft {
        hetzner: env.hcloud.take(),
        cloudflare: env.cloudflare.clone(),
        name: args.name.clone(),
        email: args.email.clone(),
        ..Default::default()
    };
    if let Some(dom) = &args.domain {
        if is_auto(dom) {
            d.domain = Some(String::new());
            d.dns = Some(DnsPlan::Instant);
        } else {
            d.domain = Some(valid_domain(dom).map_err(crate::output::usage)?);
        }
    }
    // Steps answered on the command line are never asked (each step
    // takes the flag's value itself), so Back skips over them.
    let token_given = d.hetzner.is_some();
    let asked = |s: Step| match s {
        Step::Token => !token_given,
        Step::Location => args.location.is_none(),
        Step::Type => args.server_type.is_none(),
        Step::Name => args.name.is_none(),
        Step::Domain => args.domain.is_none(),
        // Interactively the address question settles DNS too.
        Step::Dns => args.domain.is_some() && !args.cloudflare_dns,
        Step::Email => args.email.is_none(),
        Step::Ssh => args.ssh_key_file.is_none() && args.ssh_key_id.is_none() && !args.no_ssh_key,
        Step::Confirm => !ctx.global.yes,
    };

    let mut i = 0;
    while let Some(&step) = STEPS.get(i) {
        let nav = match step {
            Step::Token => token_step(out, &mut d, settings).await?,
            Step::Location => location_step(out, &mut d, args)?,
            Step::Type => type_step(&mut d, args)?,
            Step::Name => name_step(&mut d, args)?,
            Step::Domain => domain_step(out, &mut d, settings, args).await?,
            Step::Dns => dns_step(out, &mut d, settings, args).await?,
            Step::Email => email_step(&mut d, args)?,
            Step::Ssh => ssh_step(&mut d, args)?,
            Step::Confirm if ctx.global.yes => Nav::Next,
            Step::Confirm => confirm_step(out, &d)?,
        };
        match nav {
            Nav::Next => i += 1,
            Nav::Back => match (0..i).rev().find(|&j| {
                // An instant address has no DNS question to go back to.
                let skipped = STEPS[j] == Step::Dns && matches!(d.dns, Some(DnsPlan::Instant));
                asked(STEPS[j]) && !skipped
            }) {
                Some(j) => {
                    if STEPS[j] == Step::Token {
                        d.hetzner = None;
                    }
                    i = j;
                }
                // Back from the first question: leave.
                None => return Ok(None),
            },
        }
    }

    let location = d.catalog[d.location.expect("location chosen")].clone();
    let server_type = location
        .find_type(d.server_type.as_deref().unwrap_or_default())
        .cloned()
        .expect("type chosen");
    let plan = Plan {
        location,
        server_type,
        name: d.name.unwrap_or_else(|| DEFAULT_NAME.into()),
        domain: d.domain.expect("domain chosen"),
        email: d.email.expect("email chosen"),
        ssh: d.ssh.unwrap_or(SshChoice::None),
        dns: d.dns.unwrap_or(DnsPlan::Manual),
        install_url: args
            .install_url
            .clone()
            .unwrap_or_else(|| cloud_init::INSTALL_URL.into()),
    };
    let secrets = Secrets {
        hetzner: d.hetzner.expect("token checked"),
        cloudflare: d.cloudflare,
    };
    Ok(Some((plan, secrets)))
}

async fn token_step(out: &Output, d: &mut Draft, settings: &Settings) -> anyhow::Result<Nav> {
    loop {
        let token = match d.hetzner.take() {
            Some(t) => t,
            None => {
                say(
                    out,
                    &format!(
                        "A Hetzner Cloud API token (Read & Write): console.hetzner.cloud → your project → {}.",
                        out.accent("Security → API tokens")
                    ),
                );
                say(
                    out,
                    &out.dim("It stays in memory and is forgotten when this finishes."),
                );
                let r = Password::new("Hetzner API token")
                    .with_display_mode(PasswordDisplayMode::Masked)
                    .without_confirmation()
                    .prompt();
                match answer(r)? {
                    Some(t) if !t.trim().is_empty() => SecretString::from(t.trim().to_owned()),
                    Some(_) => continue,
                    None => return Ok(Nav::Back),
                }
            }
        };
        out.note(format!(
            "  {}",
            out.dim("Checking the token and what's in stock…")
        ));
        let hz = flow::hetzner(settings, &token)?;
        let loaded = async {
            let catalog = load_catalog(&hz).await?;
            let keys = hz.ssh_keys().await.map_err(flow::hetzner_error)?;
            anyhow::Ok((catalog, keys))
        }
        .await;
        match loaded {
            Ok((catalog, keys)) if !catalog.is_empty() => {
                d.catalog = catalog;
                d.ssh_keys = keys;
                d.hetzner = Some(token);
                return Ok(Nav::Next);
            }
            Ok(_) => {
                anyhow::bail!("Hetzner has no suitable server type in stock anywhere right now")
            }
            Err(e) => {
                out.note(format!("  ✗ {e:#}"));
                // A bad token from the environment can't be fixed here.
                if std::env::var_os(super::ENV_HCLOUD).is_some() {
                    return Err(e);
                }
            }
        }
    }
}

fn location_step(out: &Output, d: &mut Draft, args: &ServerCreateArgs) -> anyhow::Result<Nav> {
    if let Some(wanted) = &args.location {
        let i = d
            .catalog
            .iter()
            .position(|l| l.name.eq_ignore_ascii_case(wanted))
            .ok_or_else(|| {
                crate::output::usage(format!(
                    "--location {wanted}: nothing suitable can be created there now. Available: {}",
                    d.catalog
                        .iter()
                        .map(|l| l.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;
        d.location = Some(i);
        return Ok(Nav::Next);
    }
    let options: Vec<Choice<usize>> = d
        .catalog
        .iter()
        .enumerate()
        .map(|(i, l)| Choice {
            label: format!("{:<24} {}", l.label(), l.offer()),
            value: i,
        })
        .collect();
    let start = d
        .location
        .or_else(|| d.catalog.iter().position(|l| l.name == "fsn1"))
        .unwrap_or(0);
    say(
        out,
        &out.dim(
            "Cheapest server per location (€/mo incl. VAT). Outside Europe costs more and has no Arm.\n  You pick the size next.",
        ),
    );
    let r = Select::new("Where should it run?", options)
        .with_starting_cursor(start)
        .with_page_size(10)
        .with_help_message(
            "only locations with a suitable server in stock · type to filter · Esc: back",
        )
        .prompt();
    match answer(r)? {
        Some(c) => {
            if d.location != Some(c.value) {
                d.server_type = None;
            }
            d.location = Some(c.value);
            Ok(Nav::Next)
        }
        None => Ok(Nav::Back),
    }
}

fn type_step(d: &mut Draft, args: &ServerCreateArgs) -> anyhow::Result<Nav> {
    let loc = &d.catalog[d.location.expect("location first")];
    if let Some(wanted) = &args.server_type {
        let t = loc.find_type(wanted).ok_or_else(|| {
            crate::output::usage(format!(
                "--type {wanted} isn't available in {} now. Available there: {}",
                loc.name,
                loc.types
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        d.server_type = Some(t.name.clone());
        return Ok(Nav::Next);
    }
    let options: Vec<Choice<String>> = loc
        .types
        .iter()
        .map(|t| Choice {
            label: t.label(),
            value: t.name.clone(),
        })
        .collect();
    let start = d
        .server_type
        .as_deref()
        .or(loc.recommended.as_deref())
        .and_then(|n| loc.types.iter().position(|t| t.name == n))
        .unwrap_or(0);
    let help = format!(
        "★ = enough for {} · prices incl. VAT · type to filter · Esc: back",
        catalog::RECOMMENDATION
    );
    let prompt = format!("Which server in {}?", loc.label());
    let r = Select::new(&prompt, options)
        .with_starting_cursor(start)
        .with_page_size(12)
        .with_help_message(&help)
        .prompt();
    match answer(r)? {
        Some(c) => {
            d.server_type = Some(c.value);
            Ok(Nav::Next)
        }
        None => Ok(Nav::Back),
    }
}

fn validator(
    check: fn(&str) -> Result<String, String>,
) -> impl Fn(&str) -> Result<Validation, inquire::CustomUserError> + Clone {
    move |s: &str| {
        Ok(match check(s) {
            Ok(_) => Validation::Valid,
            Err(e) => Validation::Invalid(e.into()),
        })
    }
}

fn name_step(d: &mut Draft, args: &ServerCreateArgs) -> anyhow::Result<Nav> {
    if let Some(n) = &args.name {
        d.name =
            Some(valid_server_name(n).map_err(|e| crate::output::usage(format!("--name: {e}")))?);
        return Ok(Nav::Next);
    }
    let default = d.name.clone().unwrap_or_else(|| DEFAULT_NAME.into());
    let r = Text::new("Server name")
        .with_default(&default)
        .with_help_message("its hostname, and its name on the Servers page · Esc: back")
        .with_validator(validator(valid_server_name))
        .prompt();
    match answer(r)? {
        Some(n) => {
            d.name = Some(n.trim().to_owned());
            Ok(Nav::Next)
        }
        None => Ok(Nav::Back),
    }
}

/// A Cloudflare API token that can edit DNS, prefilled on Cloudflare's
/// "Create token" page.
const CLOUDFLARE_TOKEN_URL: &str = "https://dash.cloudflare.com/profile/api-tokens?permissionGroupKeys=%5B%7B%22key%22%3A%22zone%22%2C%22type%22%3A%22read%22%7D%2C%7B%22key%22%3A%22dns%22%2C%22type%22%3A%22edit%22%7D%5D&name=Modula%20Deploy";

#[derive(Clone, Copy, PartialEq)]
enum Address {
    Cloudflare,
    OwnDomain,
    Ip,
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Address::Cloudflare => {
                "A domain on Cloudflare: pick it, the DNS record is created for you"
            }
            Address::OwnDomain => "Another domain: I'll add the DNS record myself",
            Address::Ip => "No domain yet: https://<server-ip> (add one later in the dashboard)",
        })
    }
}

/// Where the dashboard lives: a Cloudflare domain (connect, pick the zone,
/// then a subdomain), a domain whose DNS the user handles, or the bare IP.
async fn domain_step(
    out: &Output,
    d: &mut Draft,
    settings: &Settings,
    args: &ServerCreateArgs,
) -> anyhow::Result<Nav> {
    if args.domain.is_some() {
        return Ok(Nav::Next);
    }
    loop {
        let options = vec![Address::Cloudflare, Address::OwnDomain, Address::Ip];
        let start = match d.dns {
            Some(DnsPlan::Manual) => 1,
            Some(DnsPlan::Instant) => 2,
            _ => 0,
        };
        let r = Select::new("Dashboard address", options)
            .with_starting_cursor(start)
            .without_filtering()
            .with_help_message("Esc: back")
            .prompt();
        let Some(choice) = answer(r)? else {
            return Ok(Nav::Back);
        };
        match choice {
            Address::Ip => {
                d.domain = Some(String::new());
                d.dns = Some(DnsPlan::Instant);
                return Ok(Nav::Next);
            }
            Address::OwnDomain => {
                let mut t = Text::new("Dashboard domain")
                    .with_help_message(
                        "e.g. deploy.example.com; you'll add its A record once the server has its IP · Esc: back",
                    )
                    .with_validator(validator(valid_domain));
                if let (Some(dom), Some(DnsPlan::Manual)) = (&d.domain, &d.dns) {
                    t = t.with_initial_value(dom);
                }
                if let Some(dom) = answer(t.prompt())? {
                    d.domain = Some(valid_domain(&dom).map_err(anyhow::Error::msg)?);
                    d.dns = Some(DnsPlan::Manual);
                    return Ok(Nav::Next);
                }
            }
            Address::Cloudflare => {
                if cloudflare_domain(out, d, settings, args).await? {
                    return Ok(Nav::Next);
                }
            }
        }
    }
}

/// Connect Cloudflare, pick a zone and a subdomain. `false` when the user
/// backed out (to the address question).
async fn cloudflare_domain(
    out: &Output,
    d: &mut Draft,
    settings: &Settings,
    args: &ServerCreateArgs,
) -> anyhow::Result<bool> {
    // Connect: a token from the environment or this session, else ask.
    let (cf, zones) = loop {
        let token = match d.cloudflare.clone() {
            Some(t) => t,
            None => {
                say(
                    out,
                    &format!(
                        "Create a Cloudflare API token with {} (opening Cloudflare):",
                        out.accent("Zone → Read and DNS → Edit")
                    ),
                );
                say(out, &out.dim(CLOUDFLARE_TOKEN_URL));
                crate::commands::project::open_in_browser(CLOUDFLARE_TOKEN_URL);
                let r = Password::new("Cloudflare API token")
                    .with_display_mode(PasswordDisplayMode::Masked)
                    .without_confirmation()
                    .with_help_message("kept in memory only · Esc: back")
                    .prompt();
                match answer(r)? {
                    Some(t) if !t.trim().is_empty() => SecretString::from(t.trim().to_owned()),
                    Some(_) => continue,
                    None => return Ok(false),
                }
            }
        };
        let cf = cloudflare::Client::new(settings.cloudflare_api.clone(), token.clone())?;
        let listed = async {
            cf.verify().await?;
            cf.zones().await
        }
        .await;
        match listed {
            Ok(zones) if !zones.is_empty() => {
                d.cloudflare = Some(token);
                break (cf, zones);
            }
            Ok(_) => out.note(format!(
                "  ✗ {}",
                "the token can't see any domain (give it Zone → Read for your domains)"
            )),
            Err(e) => out.note(format!("  ✗ {e:#}")),
        }
        d.cloudflare = None;
        if std::env::var_os(super::ENV_CLOUDFLARE).is_some() {
            anyhow::bail!(
                "{} can't list your Cloudflare domains",
                super::ENV_CLOUDFLARE
            );
        }
    };

    loop {
        let names: Vec<String> = zones.iter().map(|z| z.name.clone()).collect();
        let start = d
            .domain
            .as_deref()
            .and_then(|dom| {
                names
                    .iter()
                    .position(|z| dom == z || dom.ends_with(&format!(".{z}")))
            })
            .unwrap_or(0);
        let r = Select::new("Domain", names)
            .with_starting_cursor(start)
            .with_page_size(12)
            .with_help_message("your Cloudflare domains · type to filter · Esc: back")
            .prompt();
        let Some(zone_name) = answer(r)? else {
            return Ok(false);
        };
        let zone = zones
            .iter()
            .find(|z| z.name == zone_name)
            .expect("listed")
            .clone();

        let default_sub = d
            .domain
            .as_deref()
            .and_then(|dom| dom.strip_suffix(&format!(".{zone_name}")))
            .unwrap_or("deploy")
            .to_owned();
        let r = Text::new("Subdomain")
            .with_default(&default_sub)
            .with_help_message(&format!(
                "the dashboard at <subdomain>.{zone_name} · a single - : {zone_name} itself · Esc: back"
            ))
            .with_validator(validator(|v: &str| {
                let v = v.trim();
                if v.is_empty() || v == "-" {
                    Ok(String::new())
                } else {
                    valid_domain(&format!("{v}.example.com"))
                }
            }))
            .prompt();
        let Some(sub) = answer(r)? else {
            continue;
        };
        let sub = sub.trim().trim_end_matches('.').to_ascii_lowercase();
        let domain = if sub.is_empty() || sub == "-" {
            zone_name.clone()
        } else {
            format!("{sub}.{zone_name}")
        };

        let records = match cf.address_records(&zone.id, &domain).await {
            Ok(r) => r,
            Err(e) => {
                out.note(format!("  ✗ {e:#}"));
                continue;
            }
        };
        if !records.is_empty() && !args.replace_dns {
            let q = format!(
                "{domain} already has {} at Cloudflare. Replace with the new server's IP?",
                describe_records(&records)
            );
            if answer(Confirm::new(&q).with_default(false).prompt())? != Some(true) {
                continue;
            }
        }
        d.domain = Some(domain);
        d.dns = Some(DnsPlan::Cloudflare {
            zone,
            replace: records,
        });
        return Ok(true);
    }
}

async fn dns_step(
    out: &Output,
    d: &mut Draft,
    settings: &Settings,
    args: &ServerCreateArgs,
) -> anyhow::Result<Nav> {
    let domain = d.domain.clone().expect("domain first");
    if matches!(d.dns, Some(DnsPlan::Instant)) {
        return Ok(Nav::Next);
    }
    loop {
        let use_cf = if args.cloudflare_dns {
            true
        } else {
            let options = vec![
                Choice {
                    label: format!(
                        "I'll add the A record for {domain} myself (shown once the server has its IP)"
                    ),
                    value: false,
                },
                Choice {
                    label: "Create it with Cloudflare (an API token with Zone → DNS → Edit)".into(),
                    value: true,
                },
            ];
            let start = usize::from(
                d.cloudflare.is_some() || matches!(d.dns, Some(DnsPlan::Cloudflare { .. })),
            );
            let r = Select::new("DNS for the dashboard", options)
                .with_starting_cursor(start)
                .without_filtering()
                .with_help_message(
                    "Let's Encrypt needs the domain to point at the server · Esc: back",
                )
                .prompt();
            match answer(r)? {
                Some(c) => c.value,
                None => return Ok(Nav::Back),
            }
        };
        if !use_cf {
            d.dns = Some(DnsPlan::Manual);
            return Ok(Nav::Next);
        }
        let token = match d.cloudflare.clone() {
            Some(t) => t,
            None => {
                let r = Password::new("Cloudflare API token")
                    .with_display_mode(PasswordDisplayMode::Masked)
                    .without_confirmation()
                    .with_help_message("kept in memory only · Esc: back")
                    .prompt();
                match answer(r)? {
                    Some(t) if !t.trim().is_empty() => SecretString::from(t.trim().to_owned()),
                    _ => continue,
                }
            }
        };
        let cf = cloudflare::Client::new(settings.cloudflare_api.clone(), token.clone())?;
        match cloudflare_lookup(&cf, &domain).await {
            Ok((zone, records)) => {
                if !records.is_empty() && !args.replace_dns {
                    let q = format!(
                        "{domain} already has {} at Cloudflare. Replace with the new server's IP?",
                        describe_records(&records)
                    );
                    match answer(Confirm::new(&q).with_default(false).prompt())? {
                        Some(true) => {}
                        _ => continue,
                    }
                }
                d.cloudflare = Some(token);
                d.dns = Some(DnsPlan::Cloudflare {
                    zone,
                    replace: records,
                });
                return Ok(Nav::Next);
            }
            Err(e) => {
                out.note(format!("  ✗ {e:#}"));
                d.cloudflare = None;
                if args.cloudflare_dns {
                    return Err(e);
                }
            }
        }
    }
}

fn email_step(d: &mut Draft, args: &ServerCreateArgs) -> anyhow::Result<Nav> {
    if let Some(e) = &args.email {
        d.email = Some(valid_email(e).map_err(|e| crate::output::usage(format!("--email: {e}")))?);
        return Ok(Nav::Next);
    }
    let mut t = Text::new("Your email")
        .with_help_message("for TLS certificates (Let's Encrypt) and your admin login · Esc: back")
        .with_validator(validator(valid_email));
    if let Some(e) = &d.email {
        t = t.with_initial_value(e);
    }
    match answer(t.prompt())? {
        Some(e) => {
            d.email = Some(valid_email(&e).map_err(anyhow::Error::msg)?);
            Ok(Nav::Next)
        }
        None => Ok(Nav::Back),
    }
}

fn ssh_step(d: &mut Draft, args: &ServerCreateArgs) -> anyhow::Result<Nav> {
    if let Some(path) = &args.ssh_key_file {
        let key = read_public_key(path)
            .map_err(|e| crate::output::usage(format!("--ssh-key-file: {e:#}")))?;
        d.ssh = Some(ssh_choice_for_file(path, key, &d.ssh_keys));
        return Ok(Nav::Next);
    }
    if let Some(id) = args.ssh_key_id {
        let k = d.ssh_keys.iter().find(|k| k.id == id).ok_or_else(|| {
            crate::output::usage(format!("--ssh-key-id {id}: no such key in this project"))
        })?;
        d.ssh = Some(SshChoice::Existing {
            id: k.id,
            name: k.name.clone(),
        });
        return Ok(Nav::Next);
    }
    if args.no_ssh_key {
        d.ssh = Some(SshChoice::None);
        return Ok(Nav::Next);
    }
    let mut options: Vec<Choice<SshChoice>> = Vec::new();
    let local = local_public_keys();
    for (path, key) in &local {
        let choice = ssh_choice_for_file(path, key.clone(), &d.ssh_keys);
        let shown = path
            .display()
            .to_string()
            .replace(&std::env::var("HOME").unwrap_or_default(), "~");
        let label = match &choice {
            SshChoice::Existing { name, .. } => {
                format!("{shown}  (already in Hetzner as “{name}”)")
            }
            _ => format!("{shown}  (upload)"),
        };
        options.push(Choice {
            label,
            value: choice,
        });
    }
    let local_bodies: Vec<String> = local.iter().map(|(_, k)| key_body(k)).collect();
    for k in d
        .ssh_keys
        .iter()
        .filter(|k| !local_bodies.contains(&key_body(&k.public_key)))
    {
        options.push(Choice {
            label: format!("“{}” from your Hetzner project  {}", k.name, k.fingerprint),
            value: SshChoice::Existing {
                id: k.id,
                name: k.name.clone(),
            },
        });
    }
    options.push(Choice {
        label: "No SSH key (Hetzner emails you a root password; not recommended)".into(),
        value: SshChoice::None,
    });
    let start = d
        .ssh
        .as_ref()
        .and_then(|s| options.iter().position(|o| &o.value == s))
        .unwrap_or(0);
    let r = Select::new("SSH key for root", options)
        .with_starting_cursor(start)
        .with_help_message("for logging in to the server later · Esc: back")
        .prompt();
    match answer(r)? {
        Some(c) => {
            d.ssh = Some(c.value);
            Ok(Nav::Next)
        }
        None => Ok(Nav::Back),
    }
}

fn confirm_step(out: &Output, d: &Draft) -> anyhow::Result<Nav> {
    let loc = &d.catalog[d.location.expect("location")];
    let t = loc
        .find_type(d.server_type.as_deref().unwrap_or_default())
        .expect("type");
    let domain = d.domain.as_deref().unwrap_or_default();
    let bar = out.accent("│");
    let row = |k: &str, v: String| out.note(format!("  {bar}  {k:<13}{v}"));
    out.note("");
    out.note(format!(
        "  {} {}",
        out.accent("╭─"),
        "Your new Modula server"
    ));
    out.note(format!("  {bar}"));
    row(
        "Name",
        d.name.clone().unwrap_or_else(|| DEFAULT_NAME.into()),
    );
    row("Where", loc.label());
    row("Server", t.summary());
    if matches!(d.dns, Some(DnsPlan::Instant)) {
        row(
            "Dashboard",
            out.accent("https://<server-ip> (no domain yet; add one in the dashboard)"),
        );
    } else {
        row("Dashboard", out.accent(&format!("https://{domain}")));
    }
    row(
        "DNS",
        match &d.dns {
            Some(DnsPlan::Cloudflare { zone, replace }) if !replace.is_empty() => {
                format!(
                    "Cloudflare creates the A record in {} (replacing {})",
                    zone.name,
                    describe_records(replace)
                )
            }
            Some(DnsPlan::Cloudflare { zone, .. }) => {
                format!("Cloudflare creates the A record in {}", zone.name)
            }
            Some(DnsPlan::Instant) => {
                "nothing to set up yet: the dashboard uses the server's IP".into()
            }
            _ => "you add an A record → the server's IP (shown then)".into(),
        },
    );
    row("Email", d.email.clone().unwrap_or_default());
    row(
        "SSH",
        match &d.ssh {
            Some(SshChoice::Existing { name, .. }) => format!("key “{name}”"),
            Some(SshChoice::Upload { path, .. }) => format!("upload {}", path.display()),
            _ => "none (root password by email)".into(),
        },
    );
    row(
        "Firewall",
        "SSH, 80, 443 (tcp+udp), 20000–29999, 7101".into(),
    );
    row(
        "Cost",
        format!(
            "{} {}",
            out.accent(&t.monthly),
            out.dim("per month incl. VAT, billed by Hetzner")
        ),
    );
    out.note(format!("  {bar}"));
    out.note(format!(
        "  {} {}",
        out.accent("╰─"),
        out.dim("Takes about 5–10 minutes. You'll set your admin password at the end.")
    ));
    out.note("");
    let q = format!("Create it for {}/month?", t.monthly);
    match answer(
        Confirm::new(&q)
            .with_default(true)
            .with_help_message("Esc: back to change something")
            .prompt(),
    )? {
        Some(true) => Ok(Nav::Next),
        Some(false) => Err(Quit.into()),
        None => Ok(Nav::Back),
    }
}

/// The admin's name and password, asked once the server is up.
pub fn ask_admin(
    email: &str,
    name: Option<&str>,
    password: Option<SecretString>,
) -> anyhow::Result<(String, SecretString)> {
    inquire::set_global_render_config(theme());
    let name = match name {
        Some(n) => valid_admin_name(n).map_err(anyhow::Error::msg)?,
        None => {
            let r = Text::new("Your name")
                .with_default(DEFAULT_ADMIN_NAME)
                .with_help_message(&format!("for the admin account {email}"))
                .with_validator(validator(valid_admin_name))
                .prompt();
            answer(r)?.ok_or(Quit)?.trim().to_owned()
        }
    };
    let password = match password {
        Some(p) => p,
        None => {
            let r = Password::new("Admin password")
                .with_display_mode(PasswordDisplayMode::Masked)
                .with_custom_confirmation_message("Once more")
                .with_help_message("at least 10 characters")
                .with_validator(|s: &str| {
                    Ok(match valid_password(s) {
                        Ok(()) => Validation::Valid,
                        Err(e) => Validation::Invalid(e.into()),
                    })
                })
                .prompt();
            SecretString::from(answer(r)?.ok_or(Quit)?)
        }
    };
    valid_password(password.expose_secret()).map_err(anyhow::Error::msg)?;
    Ok((name, password))
}

/// After a failure: delete what was created? (Default yes.)
pub fn ask_delete(plan: &Plan, created: &Created) -> anyhow::Result<bool> {
    let what = if created.server.is_some() {
        format!(
            "Delete server {} and its firewall so it stops billing?",
            plan.name
        )
    } else {
        "Delete the firewall that was created?".into()
    };
    let r = Confirm::new(&what)
        .with_default(true)
        .with_help_message("No keeps it, e.g. to look at /var/log/cloud-init-output.log over SSH")
        .prompt();
    Ok(answer(r)?.unwrap_or(true))
}
