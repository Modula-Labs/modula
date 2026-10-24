//! `ssh` (a shell or command in the running container) and `run` (a local
//! command with the service's variables).

use anyhow::{Context, bail};

use crate::context::Ctx;
use crate::resolve;
use crate::shell::{self, ExecOptions};

pub async fn ssh(
    ctx: &Ctx,
    service: Option<String>,
    replica: Option<u16>,
    command: Vec<String>,
) -> anyhow::Result<i32> {
    let api = ctx.api()?;
    let project = ctx.project().await?;
    // The positional service wins; then -s; then the linked one.
    let service = match service.as_deref() {
        Some(name) => resolve::default_service(&project.services, Some(name))?,
        None => ctx.service(&project)?,
    }
    .clone();
    let t = ctx.target_with(project, service)?;
    let deployment = api
        .deployments(&t.instance)
        .await?
        .into_iter()
        .find(|d| d.status == "active")
        .with_context(|| {
            format!(
                "{} has no running deployment in {}; deploy it first",
                t.service.name, t.environment.name
            )
        })?;
    let opts = ExecOptions {
        cmd: command,
        tty: shell::wants_tty(),
        replica,
        ..Default::default()
    };
    shell::attach(&api.login, &deployment.id, opts).await
}

pub async fn run(ctx: &Ctx, command: Vec<String>) -> anyhow::Result<i32> {
    let Some((program, args)) = command.split_first() else {
        bail!("no command to run");
    };
    let api = ctx.api()?;
    let t = ctx.target().await?;
    let env = api.run_env(&t.instance).await?;
    if !env.withheld.is_empty() {
        ctx.out.note(format!(
            "Not set (sealed, or referencing a sealed variable): {}",
            env.withheld.join(", ")
        ));
    }
    if !env.missing.is_empty() {
        ctx.out.note(format!(
            "Referenced but not defined (empty): {}",
            env.missing.join(", ")
        ));
    }
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .envs(&env.variables)
        .spawn()
        .with_context(|| format!("cannot run {program}"))?;
    drop(env);
    // Ctrl-C reaches the command (same process group); we only wait for it.
    let status = loop {
        tokio::select! {
            status = child.wait() => break status?,
            _ = tokio::signal::ctrl_c() => {}
        }
    };
    Ok(exit_code(status))
}

/// The command's exit code; killed by a signal = 128 + the signal (shells do
/// the same).
fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn exit_codes_follow_the_shell_convention() {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(exit_code(std::process::ExitStatus::from_raw(3 << 8)), 3);
        assert_eq!(exit_code(std::process::ExitStatus::from_raw(9)), 137);
    }
}
