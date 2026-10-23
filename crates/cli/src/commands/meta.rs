//! `completion` and `docs`: generated from the command definitions, so they
//! never drift from what the binary accepts.

use clap::{Arg, Command, CommandFactory};

use crate::cli::Cli;
use crate::context::Ctx;

pub fn completion(ctx: &Ctx, shell: clap_complete::Shell) {
    let mut buf = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), "modula-deploy", &mut buf);
    ctx.out.line(String::from_utf8_lossy(&buf).trim_end());
}

fn flag(a: &Arg) -> String {
    let mut names = Vec::new();
    if let Some(s) = a.get_short() {
        names.push(format!("-{s}"));
    }
    if let Some(l) = a.get_long() {
        names.push(format!("--{l}"));
    }
    let takes_value = a.get_action().takes_values();
    let value = a
        .get_value_names()
        .and_then(|v| v.first())
        .map(|v| v.to_string())
        .unwrap_or_else(|| a.get_id().as_str().to_uppercase());
    match (names.is_empty(), takes_value) {
        (true, _) if a.is_required_set() => format!("<{value}>"),
        (true, _) => format!("[{value}]"),
        (false, true) => format!("{} <{value}>", names.join(", ")),
        (false, false) => names.join(", "),
    }
}

fn help(a: &Arg) -> String {
    let mut text = a
        .get_help()
        .map(|h| h.to_string())
        .unwrap_or_default()
        .replace('\n', " ");
    if !text.is_empty() && !text.ends_with(['.', ')']) {
        text.push('.');
    }
    let takes_values = a.get_action().takes_values();
    let values: Vec<String> = a
        .get_possible_values()
        .iter()
        .filter(|v| !v.is_hide_set())
        .map(|v| v.get_name().to_owned())
        .collect();
    if !values.is_empty() && takes_values {
        text.push_str(&format!(" One of: {}.", values.join(", ")));
    }
    let defaults: Vec<String> = a
        .get_default_values()
        .iter()
        .map(|v| v.to_string_lossy().into_owned())
        .collect();
    if !defaults.is_empty() && takes_values {
        text.push_str(&format!(" Default: {}.", defaults.join(", ")));
    }
    if let Some(env) = a.get_env() {
        text.push_str(&format!(" Env: {}.", env.to_string_lossy()));
    }
    text
}

/// Documented once, under "Global flags".
const TOP_LEVEL_GLOBALS: &[&str] = &["json", "project", "environment", "service", "yes"];

fn document(cmd: &Command, path: &str, out: &mut String, depth: usize) {
    let args: Vec<&Arg> = cmd
        .get_arguments()
        .filter(|a| {
            let id = a.get_id().as_str();
            let documented_above = depth > 0 && TOP_LEVEL_GLOBALS.contains(&id);
            !(a.is_hide_set() || documented_above || matches!(id, "help" | "version"))
        })
        .collect();
    if depth > 0 {
        out.push_str(&format!("\n### `{path}`\n\n"));
        if let Some(about) = cmd.get_about() {
            out.push_str(&format!("{about}\n"));
        }
        let aliases: Vec<&str> = cmd.get_visible_aliases().collect();
        if !aliases.is_empty() {
            out.push_str(&format!("Aliases: {}.\n", aliases.join(", ")));
        }
        if !args.is_empty() {
            out.push('\n');
            for a in &args {
                out.push_str(&format!("- `{}`: {}\n", flag(a), help(a).trim()));
            }
        }
    }
    for sub in cmd.get_subcommands().filter(|s| !s.is_hide_set()) {
        if sub.get_name() == "help" {
            continue;
        }
        document(sub, &format!("{path} {}", sub.get_name()), out, depth + 1);
    }
}

/// The whole command reference as Markdown.
pub fn reference() -> String {
    let cmd = Cli::command();
    let mut out = format!(
        "# modula-deploy {}\n\n{}\n\n## Global flags\n\n",
        cmd.get_version().unwrap_or_default(),
        cmd.get_about().map(|a| a.to_string()).unwrap_or_default()
    );
    for a in cmd
        .get_arguments()
        .filter(|a| a.is_global_set() && !a.is_hide_set())
    {
        out.push_str(&format!("- `{}`: {}\n", flag(a), help(a).trim()));
    }
    out.push_str("\n## Commands\n");
    document(&cmd, "modula-deploy", &mut out, 0);
    out
}

pub fn docs(ctx: &Ctx) {
    ctx.out.line(reference().trim_end());
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_reference_covers_every_command() {
        let text = super::reference();
        for needle in [
            "### `modula-deploy variables set`",
            "### `modula-deploy environment new`",
            "### `modula-deploy db query`",
            "- `--json`:",
            "- `-s, --service <SERVICE>`:",
            "Env: MODULA_SERVICE.",
            "- `-d, --database <DATABASE>`: A database from its template. One of: postgres, mysql, redis, mongo.",
            "- `--shared`: The environment's shared variables",
        ] {
            assert!(text.contains(needle), "missing {needle}:\n{text}");
        }
        // Hidden compatibility flags stay out of the docs.
        assert!(!text.contains("skip-deploys"));
    }
}
