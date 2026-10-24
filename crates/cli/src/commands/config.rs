//! `config init|pull`, `config plan`, `config apply`, `config validate`:
//! the project as code in `modula.toml` (see crate::iac).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Context as _, bail};
use serde::Serialize;
use serde_json::{Value, json};

use crate::api::endpoints::VariableScope;
use crate::api::{
    Api, DeployedList, Environment, NewEnvironment, NewService, ProjectDetail, VariableChanges,
};
use crate::cli::{ConfigCommand, ConfigFileArgs, ConfigPlanArgs};
use crate::commands::variables::parse_dotenv;
use crate::config::LINK_DIR;
use crate::context::Ctx;
use crate::iac::diff::{
    self, DomainChange, EnvPlan, Options, SecretSource, ServiceAction, ServicePlan, TcpProxyChange,
    VarChange, VarOp,
};
use crate::iac::model::{ConfigFile, FILES};
use crate::iac::pull;
use crate::iac::remote::RemoteEnv;
use crate::output::{Output, usage};
use crate::resolve;

pub async fn run(ctx: &mut Ctx, command: ConfigCommand) -> anyhow::Result<i32> {
    match command {
        ConfigCommand::Init { force, file } => pull_file(ctx, force, file).await.map(|()| 0),
        ConfigCommand::Plan(args) => plan(ctx, &args).await,
        ConfigCommand::Apply { plan, deploy } => apply(ctx, &plan, deploy).await,
        ConfigCommand::Validate(args) => validate(ctx, &args).map(|()| 0),
    }
}

fn load_file(ctx: &Ctx, args: &ConfigFileArgs) -> anyhow::Result<ConfigFile> {
    let path = match &args.file {
        Some(p) => ctx.session.cwd.join(p),
        None => ConfigFile::find(&ctx.session.cwd).ok_or_else(|| {
            usage(format!(
                "no {} here or above; write one with `modula config init`",
                FILES.join(" or ")
            ))
        })?,
    };
    ConfigFile::load(&path)
}

fn validate(ctx: &Ctx, args: &ConfigFileArgs) -> anyhow::Result<()> {
    let file = load_file(ctx, args)?;
    let base = file.resolve(None)?;
    let envs: Vec<&String> = file.environments.keys().collect();
    let result = json!({
        "file": file.path,
        "valid": true,
        "project": base.project,
        "services": base.services.keys().collect::<Vec<_>>(),
        "environments": envs,
    });
    ctx.out.either(&result, |o| {
        o.line(format!(
            "{} is valid: {} service(s){}",
            file.path.display(),
            base.services.len(),
            if envs.is_empty() {
                String::new()
            } else {
                format!(
                    ", overrides for {}",
                    envs.iter()
                        .map(|e| e.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        ));
    })
}

// ------------------------------------------------------------------ pull

async fn pull_file(ctx: &mut Ctx, force: bool, file: Option<PathBuf>) -> anyhow::Result<()> {
    let path = ctx
        .session
        .cwd
        .join(file.unwrap_or_else(|| PathBuf::from(FILES[0])));
    if path.extension().is_some_and(|e| e == "json") {
        return Err(usage("config init writes TOML: name the file modula.toml"));
    }
    if path.exists() && !force {
        return Err(usage(format!(
            "{} exists; pass --force to overwrite it (or see `config plan`)",
            path.display()
        )));
    }
    let api = ctx.api()?;
    let project = ctx.project().await?;
    // The base comes from the default environment (production).
    let mut order: Vec<&Environment> = project.environments.iter().collect();
    if let Some(i) = order.iter().position(|e| e.is_default) {
        let prod = order.remove(i);
        order.insert(0, prod);
    }
    let mut envs = Vec::with_capacity(order.len());
    for e in order {
        envs.push(RemoteEnv::fetch(api, &e.id).await?);
    }
    let pulled = pull::generate(&project.name, &envs)?;
    // It must read back as it was written.
    ConfigFile::parse(&path, &pulled.text).context("the generated file doesn't parse (a bug)")?;
    std::fs::write(&path, &pulled.text)
        .with_context(|| format!("cannot write {}", path.display()))?;
    let result = json!({
        "file": path,
        "project": project.name,
        "environments": envs.iter().map(|e| &e.name).collect::<Vec<_>>(),
        "guessedSecrets": pulled.guessed_secrets,
    });
    ctx.out.either(&result, |o| {
        o.line(format!(
            "Wrote {} ({}, {} environment(s))",
            path.display(),
            project.name,
            envs.len()
        ));
        if !pulled.guessed_secrets.is_empty() {
            o.note(format!(
                "Written as {{ secret = true }} (values stay on the server): {}",
                pulled.guessed_secrets.join(", ")
            ));
        }
        o.note("Review it, commit it, and check it with `modula config plan`.");
    })
}

// ------------------------------------------------------------------ plan

/// `{ secret = true }` values: the secrets file, and with --from-env the
/// process environment (which wins).
struct LocalSecrets {
    file: BTreeMap<String, String>,
    from_env: bool,
}

impl SecretSource for LocalSecrets {
    fn get(&self, name: &str) -> Option<String> {
        if self.from_env
            && let Ok(v) = std::env::var(name)
        {
            return Some(v);
        }
        self.file.get(name).cloned()
    }
}

fn secrets(
    ctx: &Ctx,
    file: &ConfigFile,
    environment: &str,
    args: &ConfigPlanArgs,
    write_gitignore: bool,
) -> anyhow::Result<LocalSecrets> {
    let path = match &args.secrets_file {
        Some(p) => ctx.session.cwd.join(p),
        None => file
            .dir()
            .join(LINK_DIR)
            .join(format!("secrets.{environment}.env")),
    };
    let values = match std::fs::read_to_string(&path) {
        Ok(text) => {
            // Secrets files belong out of git: apply fixes .gitignore, plan warns.
            if args.secrets_file.is_none() {
                if write_gitignore {
                    ctx.ignore_modula_dir(file.dir());
                } else if crate::config::gitignore_pending(file.dir()) {
                    ctx.out.note(format!(
                        "Warning: {} isn't gitignored; add .modula/ to .gitignore (apply does it)",
                        path.display()
                    ));
                }
            }
            parse_dotenv(&text).with_context(|| format!("{} is invalid", path.display()))?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && args.secrets_file.is_none() => {
            BTreeMap::new()
        }
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    Ok(LocalSecrets {
        file: values,
        from_env: args.from_env,
    })
}

/// The project: -p, else the file's `project`, else the linked one.
async fn project(ctx: &Ctx, file: &ConfigFile) -> anyhow::Result<ProjectDetail> {
    let wanted = ctx
        .global
        .project
        .as_deref()
        .or(file.project())
        .or(ctx.linked().map(|l| l.project.as_str()));
    resolve::project(ctx.api()?, wanted).await
}

/// The environments to compare: (name, the existing one).
fn targets(
    ctx: &Ctx,
    p: &ProjectDetail,
    file: &ConfigFile,
    all: bool,
) -> anyhow::Result<Vec<(String, Option<Environment>)>> {
    if all {
        let mut out: Vec<(String, Option<Environment>)> = p
            .environments
            .iter()
            .map(|e| (e.name.clone(), Some(e.clone())))
            .collect();
        for name in file.environments.keys() {
            if !out.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
                out.push((name.clone(), None));
            }
        }
        return Ok(out);
    }
    let wanted = ctx.global.environment.clone().or_else(|| {
        ctx.linked()
            .filter(|l| l.project == p.id)
            .and_then(|l| l.environment.clone())
    });
    if let Some(w) = &wanted
        && !p
            .environments
            .iter()
            .any(|e| e.id == *w || e.name.eq_ignore_ascii_case(w))
        && file.environments.contains_key(&w.to_ascii_lowercase())
    {
        return Ok(vec![(w.to_ascii_lowercase(), None)]);
    }
    let e = resolve::pick_environment(&p.environments, wanted.as_deref())?;
    Ok(vec![(e.name.clone(), Some(e.clone()))])
}

struct Planned {
    environment: Option<Environment>,
    plan: EnvPlan,
}

async fn compute(
    ctx: &Ctx,
    file: &ConfigFile,
    p: &ProjectDetail,
    args: &ConfigPlanArgs,
) -> anyhow::Result<Vec<Planned>> {
    let api = ctx.api()?;
    let opts = Options {
        update_sealed: args.update_sealed,
    };
    let mut out = Vec::new();
    for (name, env) in targets(ctx, p, file, args.all_envs)? {
        let spec = file.resolve(Some(&name))?;
        let remote = match &env {
            Some(e) => Some(RemoteEnv::fetch(api, &e.id).await?),
            None => None,
        };
        let secrets = secrets(ctx, file, &name, args, false)?;
        out.push(Planned {
            environment: env,
            plan: diff::plan(&name, &spec, remote.as_ref(), &secrets, opts),
        });
    }
    Ok(out)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanResult<'a> {
    project: &'a str,
    changes: bool,
    environments: Vec<&'a EnvPlan>,
}

fn show(v: &Value) -> String {
    match v {
        Value::Null => "default".into(),
        other => other.to_string(),
    }
}

fn quoted(v: Option<&str>) -> String {
    v.map_or_else(|| "?".into(), |v| format!("{v:?}"))
}

fn print_variable(o: &Output, indent: &str, scope: &str, v: &VarChange) {
    let what = format!(
        "{scope}{}{}",
        v.name,
        match (v.secret, v.sealed) {
            (_, true) => " (sealed)",
            (true, false) => " (secret)",
            _ => "",
        }
    );
    match v.op {
        VarOp::Add if v.secret => o.line(format!("{indent}+ {what}")),
        VarOp::Add => o.line(format!("{indent}+ {what} = {}", quoted(v.to.as_deref()))),
        VarOp::Update if v.secret => o.line(format!("{indent}~ {what} (new value)")),
        VarOp::Update => o.line(format!(
            "{indent}~ {what}: {} → {}",
            quoted(v.from.as_deref()),
            quoted(v.to.as_deref())
        )),
        VarOp::Delete => o.line(format!("{indent}- {what}")),
    }
}

fn print_service(o: &Output, s: &ServicePlan) {
    if s.is_empty() {
        return;
    }
    match s.action {
        ServiceAction::Create => {
            let from = match &s.template {
                Some(t) => format!("template {t}"),
                None => s
                    .config
                    .get("source")
                    .map(|src| match (src.get("repo"), src.get("image")) {
                        (Some(r), _) => format!(
                            "{}@{}",
                            r.as_str().unwrap_or("?"),
                            src.get("branch").and_then(Value::as_str).unwrap_or("?")
                        ),
                        (_, Some(i)) => i.as_str().unwrap_or("?").to_owned(),
                        _ => "?".into(),
                    })
                    .unwrap_or_default(),
            };
            o.line(format!("  + service {} ({from})", s.service));
            if let Some(cfg) = s.config.as_object() {
                for (k, v) in diff::normalize(&Value::Object(cfg.clone())) {
                    if k != "source" {
                        o.line(format!("      {k} = {v}"));
                    }
                }
            }
        }
        ServiceAction::Update | ServiceAction::Unchanged => {
            o.line(format!("  ~ service {}", s.service));
        }
    }
    for c in &s.settings {
        o.line(format!(
            "      ~ {}: {} → {}",
            c.key,
            show(&c.from),
            show(&c.to)
        ));
    }
    if let Some(v) = &s.volume {
        o.line(format!("      + volume {v}"));
    }
    for t in &s.tcp_proxy {
        match t {
            TcpProxyChange::Add { port } => o.line(format!("      + TCP proxy → port {port}")),
            TcpProxyChange::Delete { port, .. } => {
                o.line(format!("      - TCP proxy → port {port}"))
            }
        }
    }
    for v in &s.variables {
        print_variable(o, "      ", "variable ", v);
    }
    for d in &s.domains {
        match d {
            DomainChange::Add { host, port } => o.line(format!(
                "      + domain {host}{}",
                port.map(|p| format!(" → port {p}")).unwrap_or_default()
            )),
            DomainChange::Update { host, from, to, .. } => o.line(format!(
                "      ~ domain {host}: port {} → {}",
                from.map_or("default".into(), |p| p.to_string()),
                to.map_or("default".into(), |p| p.to_string())
            )),
            DomainChange::Delete { host, .. } => o.line(format!("      - domain {host}")),
            DomainChange::Generate => o.line("      + generated domain"),
        }
    }
}

fn print_plans(o: &Output, project: &str, plans: &[Planned]) {
    let (mut add, mut change, mut delete) = (0, 0, 0);
    for Planned { plan: p, .. } in plans {
        let (a, c, d) = p.counts();
        add += a;
        change += c;
        delete += d;
        let status = if p.create_environment {
            " (new: will be created)"
        } else if p.is_empty() {
            ": no changes"
        } else {
            ""
        };
        o.line(format!("Environment {}/{}{status}", project, p.environment));
        for s in &p.services {
            print_service(o, s);
        }
        for v in &p.shared {
            print_variable(o, "  ", "shared variable ", v);
        }
        for m in &p.manual {
            o.line(format!("  ! manual: {m}"));
        }
        for w in &p.warnings {
            o.line(format!("  warning: {w}"));
        }
        for e in &p.errors {
            o.line(format!("  error: {e}"));
        }
    }
    o.line(format!(
        "Plan: {add} to add, {change} to change, {delete} to delete."
    ));
}

async fn plan(ctx: &Ctx, args: &ConfigPlanArgs) -> anyhow::Result<i32> {
    let file = load_file(ctx, &args.file)?;
    let p = project(ctx, &file).await?;
    let plans = compute(ctx, &file, &p, args).await?;
    let failed = plans.iter().any(|x| !x.plan.errors.is_empty());
    let result = PlanResult {
        project: &p.name,
        changes: plans.iter().any(|x| !x.plan.is_empty()),
        environments: plans.iter().map(|x| &x.plan).collect(),
    };
    ctx.out
        .either(&result, |o| print_plans(o, &p.name, &plans))?;
    Ok(i32::from(failed))
}

// ------------------------------------------------------------------ apply

async fn apply(ctx: &Ctx, args: &ConfigPlanArgs, deploy: bool) -> anyhow::Result<i32> {
    let api = ctx.api()?;
    let file = load_file(ctx, &args.file)?;
    let mut p = project(ctx, &file).await?;
    let plans = compute(ctx, &file, &p, args).await?;
    let errors: Vec<&String> = plans.iter().flat_map(|x| &x.plan.errors).collect();
    if !ctx.out.json {
        print_plans(&ctx.out, &p.name, &plans);
    }
    if !errors.is_empty() {
        bail!(
            "nothing was applied: {}",
            errors
                .iter()
                .map(|e| e.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    if plans.iter().all(|x| x.plan.is_empty()) {
        if ctx.out.json {
            ctx.out
                .json(&json!({ "project": p.name, "changes": false, "environments": [] }))?;
        }
        return Ok(0);
    }
    ctx.confirm(&format!("Apply these changes to {}?", p.name))?;

    // Environments, then services (a new service exists in every environment).
    for x in &plans {
        if x.environment.is_none() {
            api.create_environment(
                &p.id,
                &NewEnvironment {
                    name: x.plan.environment.clone(),
                    duplicate_from: None,
                    branches: Default::default(),
                },
            )
            .await?;
            ctx.out
                .note(format!("Created environment {}", x.plan.environment));
        }
    }
    let mut created = BTreeSet::new();
    for s in plans.iter().flat_map(|x| &x.plan.services) {
        if s.action != ServiceAction::Create || !created.insert(s.service.clone()) {
            continue;
        }
        let req = NewService {
            name: s.service.clone(),
            template: s.template.clone(),
            config: s.template.is_none().then(|| s.config.clone()),
        };
        api.create_service(&p.id, &req).await?;
        ctx.out.note(format!("Created service {}", s.service));
    }

    // Then each environment against what is there now.
    p = api.project(&p.id).await?;
    let opts = Options {
        update_sealed: args.update_sealed,
    };
    let mut results = Vec::new();
    for x in &plans {
        let name = &x.plan.environment;
        let env = resolve::pick_environment(&p.environments, Some(name))?.clone();
        let remote = RemoteEnv::fetch(api, &env.id).await?;
        let spec = file.resolve(Some(name))?;
        let secrets = secrets(ctx, &file, name, args, true)?;
        let fresh = diff::plan(name, &spec, Some(&remote), &secrets, opts);
        if !fresh.errors.is_empty() {
            bail!("{name}: {}", fresh.errors.join("; "));
        }
        let changed = execute(api, &env, &fresh).await?;
        let deployed = if deploy && (!changed.is_empty() || !fresh.shared.is_empty()) {
            // Shared variables reach every service that references them.
            let ids = (fresh.shared.is_empty()).then_some(changed.as_slice());
            Some(api.deploy_environment(&env.id, ids).await?)
        } else {
            None
        };
        results.push((fresh, deployed));
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Applied<'a> {
        plan: &'a EnvPlan,
        deployments: Option<&'a DeployedList>,
    }
    let out: Vec<Applied> = results
        .iter()
        .map(|(plan, d)| Applied {
            plan,
            deployments: d.as_ref(),
        })
        .collect();
    ctx.out.either(
        &json!({ "project": p.name, "changes": true, "environments": out }),
        |o| {
            for (plan, deployed) in &results {
                let (a, c, d) = plan.counts();
                o.line(format!(
                    "Applied to {}: {a} added, {c} changed, {d} deleted.",
                    plan.environment
                ));
                if let Some(list) = deployed {
                    for d in &list.deployments {
                        match (&d.deployment_id, &d.error) {
                            (Some(id), _) => o.line(format!("  deploying {} ({id})", d.service_id)),
                            (None, Some(e)) => o.line(format!("  {}: {e}", d.service_id)),
                            _ => {}
                        }
                    }
                } else if plan.services.iter().any(ServicePlan::needs_deploy)
                    || !plan.shared.is_empty()
                {
                    o.line("  Changes take effect on the next deploy (`modula deploy --all`, or apply --deploy).");
                }
                for m in &plan.manual {
                    o.line(format!("  ! still manual: {m}"));
                }
            }
        },
    )?;
    Ok(0)
}

fn changes(vars: &[VarChange]) -> VariableChanges {
    let mut c = VariableChanges::default();
    for v in vars {
        match (v.op, &v.value) {
            (VarOp::Delete, _) => c.delete.push(v.name.clone()),
            (_, Some(value)) => {
                c.set.insert(v.name.clone(), value.0.clone());
                if v.sealed {
                    c.sealed.push(v.name.clone());
                }
            }
            (_, None) => {}
        }
    }
    c
}

/// Make the changes of one environment's plan, in a safe order: settings,
/// volumes and TCP proxies, variables, then domains. Returns the ids of the
/// services whose next deployment changes.
async fn execute(api: &Api, env: &Environment, plan: &EnvPlan) -> anyhow::Result<Vec<String>> {
    let mut changed = Vec::new();
    for s in &plan.services {
        let (Some(instance), Some(service)) = (&s.instance_id, &s.service_id) else {
            bail!("{}: the service wasn't created", s.service);
        };
        if !s.settings.is_empty() {
            let _: Value = api
                .patch(
                    &format!("/api/service-instances/{instance}"),
                    &json!({ "config": s.config }),
                )
                .await
                .with_context(|| format!("updating {}'s settings", s.service))?;
        }
        if let Some(path) = &s.volume {
            api.create_volume(instance, path)
                .await
                .with_context(|| format!("adding {}'s volume", s.service))?;
        }
        for t in &s.tcp_proxy {
            match t {
                TcpProxyChange::Delete { id, .. } => {
                    api.delete_tcp_proxy(id).await?;
                }
                TcpProxyChange::Add { port } => {
                    api.create_tcp_proxy(instance, *port).await?;
                }
            }
        }
        if s.needs_deploy() {
            changed.push(service.clone());
        }
    }
    if !plan.shared.is_empty() {
        api.update_variables(VariableScope::Shared(&env.id), &changes(&plan.shared))
            .await
            .context("updating shared variables")?;
    }
    for s in &plan.services {
        let Some(instance) = &s.instance_id else {
            continue;
        };
        if !s.variables.is_empty() {
            api.update_variables(VariableScope::Instance(instance), &changes(&s.variables))
                .await
                .with_context(|| format!("updating {}'s variables", s.service))?;
        }
    }
    for s in &plan.services {
        let Some(instance) = &s.instance_id else {
            continue;
        };
        // Deletes first, so a hostname can move between services.
        for d in &s.domains {
            if let DomainChange::Delete { id, host } = d {
                api.delete_domain(id)
                    .await
                    .with_context(|| format!("deleting {host}"))?;
            }
        }
        for d in &s.domains {
            match d {
                DomainChange::Add { host, port } => {
                    api.add_custom_domain(instance, host, *port)
                        .await
                        .with_context(|| format!("adding {host}"))?;
                }
                DomainChange::Update {
                    id,
                    host,
                    to,
                    rate_limit,
                    ..
                } => {
                    api.update_domain(id, *to, *rate_limit)
                        .await
                        .with_context(|| format!("updating {host}"))?;
                }
                DomainChange::Generate => {
                    api.generate_domain(instance).await?;
                }
                DomainChange::Delete { .. } => {}
            }
        }
    }
    Ok(changed)
}
