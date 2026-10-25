//! `config init --format ts`: write `.modula/modula.ts` from the server.
//!
//! The same specs as the TOML file (see [`super::pull`]); instead of
//! overlays, a value that differs between environments becomes
//! `ctx.isEnvironment("staging") ? … : …`, with the default environment's
//! value last, so evaluating the file without an environment gives the base.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use serde_json::{Map, Value};

use super::model::CONFIG_KEYS;
use super::pull::{self, Pulled};
use super::remote::RemoteEnv;

const TEMPLATES: &[&str] = &["postgres", "mysql", "redis", "mongo", "bucket", "r2"];

/// Names a service's constant can't take.
const TAKEN: &[&str] = &[
    "defineModula",
    "project",
    "service",
    "github",
    "image",
    "local",
    "preserve",
    "secret",
    "postgres",
    "mysql",
    "redis",
    "mongo",
    "bucket",
    "r2",
    "ctx",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

/// Service keys after `source` and the settings, in writing order.
const OTHER_KEYS: &[&str] = &[
    "domains",
    "generateDomain",
    "volume",
    "tcpProxy",
    "variables",
];

/// Characters that make the CLI's `command()` run a string with `sh -c`
/// (keep in step with SHELL_SYNTAX in packages/iac/iac.js).
const SHELL_SYNTAX: &[char] = &[
    '"', '\'', '`', '$', '|', '&', ';', '<', '>', '*', '?', '(', ')', '{', '}', '[', ']', '\\',
    '~', '\n', '#',
];

struct Gen {
    /// Lowercase environment names; the first is the default.
    envs: Vec<String>,
    helpers: BTreeSet<&'static str>,
    uses_ctx: bool,
}

/// The file for `envs` (the first is the default environment); see
/// [`pull::environment_specs`] for `only`.
pub fn generate(
    project: &str,
    envs: &[RemoteEnv],
    only: Option<&BTreeSet<String>>,
) -> anyhow::Result<Pulled> {
    let (specs, guessed) = pull::environment_specs(project, envs, only)?;
    let roots: Vec<Value> = specs
        .iter()
        .map(|s| serde_json::to_value(s).map(preserve_as_variables))
        .collect::<Result<_, _>>()?;
    let mut g = Gen {
        envs: envs.iter().map(|e| e.name.to_ascii_lowercase()).collect(),
        helpers: BTreeSet::from(["defineModula", "project"]),
        uses_ctx: false,
    };
    let body = g.body(project, &roots);
    let partial = specs.iter().any(|s| s.partial);

    let mut text = String::from(
        "// Modula Deploy config as code: `modula config plan` shows what\n\
         // `config apply` would change. Secret values are never written here: put\n\
         // them in .modula/secrets.<environment>.env (gitignored) or pass --from-env.\n",
    );
    let helpers: Vec<&str> = g.helpers.iter().copied().collect();
    writeln!(
        text,
        "import {{ {} }} from \"@modula-labs/iac\";\n",
        helpers.join(", ")
    )?;
    if partial {
        text.push_str(
            "// Only this repository's services; the project's others are left alone.\n\
             export const partial = true;\n\n",
        );
    }
    writeln!(
        text,
        "export default defineModula(({}) => {{",
        if g.uses_ctx { "ctx" } else { "" }
    )?;
    text.push_str(&body);
    text.push_str("});\n");
    Ok(Pulled {
        text,
        guessed_secrets: guessed,
    })
}

/// `preserve = [...]` lists become `NAME: preserve()` variables.
fn preserve_as_variables(mut root: Value) -> Value {
    fn fold(scope: &mut Map<String, Value>) {
        let Some(Value::Array(names)) = scope.remove("preserve") else {
            return;
        };
        let vars = scope
            .entry("variables")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(vars) = vars {
            for n in names {
                if let Value::String(n) = n {
                    vars.insert(n, serde_json::json!({ "$preserve": true }));
                }
            }
        }
    }
    if let Some(Value::Object(services)) = root.get_mut("services") {
        for s in services.values_mut() {
            if let Value::Object(s) = s {
                fold(s);
            }
        }
    }
    if let Some(Value::Object(shared)) = root.get_mut("shared") {
        fold(shared);
    }
    root
}

fn ident(name: &str) -> String {
    let mut out = String::new();
    let mut upper = false;
    for c in name.chars() {
        if c == '-' {
            upper = true;
        } else if upper {
            out.push(c.to_ascii_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    if TAKEN.contains(&out.as_str()) {
        out.push_str("Service");
    }
    out
}

fn key(k: &str) -> String {
    let bare = k
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if bare {
        k.to_owned()
    } else {
        Value::String(k.to_owned()).to_string()
    }
}

fn string(s: &str) -> String {
    Value::String(s.to_owned()).to_string()
}

/// A command as the shortest form `command()` turns back into `argv`.
fn command(argv: &[Value]) -> Option<String> {
    let args: Vec<&str> = argv.iter().map(Value::as_str).collect::<Option<_>>()?;
    let plain =
        |a: &str| !a.is_empty() && !a.contains(SHELL_SYNTAX) && !a.contains(char::is_whitespace);
    match args.as_slice() {
        // `sh -c <script>` as written in a start command (nothing is run here).
        [sh, c, script]
            if *sh == "sh"
                && *c == "-c"
                && script.contains(SHELL_SYNTAX)
                && script.trim() == *script =>
        {
            Some(string(script))
        }
        args if !args.is_empty() && args.iter().all(|a| plain(a)) => Some(string(&args.join(" "))),
        _ => None,
    }
}

fn indent(n: usize) -> String {
    " ".repeat(n)
}

impl Gen {
    fn use_helper(&mut self, name: &'static str) {
        self.helpers.insert(name);
    }

    /// A value on one line.
    fn literal(&mut self, v: &Value) -> String {
        match v {
            Value::String(s) => string(s),
            Value::Array(a) => {
                let parts: Vec<String> = a.iter().map(|x| self.literal(x)).collect();
                format!("[{}]", parts.join(", "))
            }
            Value::Object(m) if m.contains_key("$preserve") => {
                self.use_helper("preserve");
                "preserve()".into()
            }
            Value::Object(m) if m.get("secret") == Some(&Value::Bool(true)) => {
                self.use_helper("secret");
                if m.get("sealed") == Some(&Value::Bool(true)) {
                    "secret({ sealed: true })".into()
                } else {
                    "secret()".into()
                }
            }
            Value::Object(m) => {
                if m.is_empty() {
                    return "{}".into();
                }
                let parts: Vec<String> = m
                    .iter()
                    .map(|(k, v)| format!("{}: {}", key(k), self.literal(v)))
                    .collect();
                format!("{{ {} }}", parts.join(", "))
            }
            other => other.to_string(),
        }
    }

    /// `ctx.isEnvironment(...)` for the environments at `indexes`.
    fn condition(&mut self, indexes: &[usize]) -> String {
        self.uses_ctx = true;
        let names: Vec<String> = indexes.iter().map(|i| string(&self.envs[*i])).collect();
        format!("ctx.isEnvironment({})", names.join(", "))
    }

    /// One expression for a value that may differ by environment; None when
    /// no environment has it.
    fn pick(
        &mut self,
        values: &[Option<&Value>],
        render: &mut dyn FnMut(&mut Self, &Value) -> String,
    ) -> Option<String> {
        if values.iter().all(Option::is_none) {
            return None;
        }
        let first = values[0];
        if values.iter().all(|v| *v == first) {
            return first.map(|v| render(self, v));
        }
        // Distinct values in environment order; the default's goes last.
        let mut groups: Vec<(Option<&Value>, Vec<usize>)> = Vec::new();
        for (i, v) in values.iter().enumerate() {
            match groups.iter_mut().find(|(g, _)| g == v) {
                Some((_, idx)) => idx.push(i),
                None => groups.push((*v, vec![i])),
            }
        }
        let (default, _) = groups.remove(0);
        let mut out = String::new();
        for (v, idx) in groups {
            let cond = self.condition(&idx);
            let value = v.map_or_else(|| "undefined".into(), |v| render(self, v));
            write!(out, "{cond} ? {value} : ").ok();
        }
        out.push_str(&default.map_or_else(|| "undefined".into(), |v| render(self, v)));
        Some(out)
    }

    fn pick_literal(&mut self, values: &[Option<&Value>]) -> Option<String> {
        self.pick(values, &mut |g, v| g.literal(v))
    }

    fn source(&mut self, v: &Value) -> String {
        if let Some(repo) = v.get("repo").and_then(Value::as_str) {
            self.use_helper("github");
            let mut opts = Vec::new();
            for k in ["branch", "rootDir"] {
                if let Some(x) = v.get(k) {
                    opts.push(format!("{k}: {}", self.literal(x)));
                }
            }
            return if opts.is_empty() {
                format!("github({})", string(repo))
            } else {
                format!("github({}, {{ {} }})", string(repo), opts.join(", "))
            };
        }
        if let Some(image) = v.get("image").and_then(Value::as_str) {
            self.use_helper("image");
            return format!("image({})", string(image));
        }
        if let Some(path) = v.get("local").and_then(Value::as_str) {
            self.use_helper("local");
            return if path == "." {
                "local()".into()
            } else {
                format!("local({})", string(path))
            };
        }
        self.literal(v)
    }

    /// `source` per environment: one `github(repo, { branch: … })` when
    /// only the branch or root differs.
    fn pick_source(&mut self, values: &[Option<&Value>]) -> Option<String> {
        let repo = values[0].and_then(|v| v.get("repo"));
        let same_repo =
            repo.is_some() && values.iter().all(|v| v.and_then(|v| v.get("repo")) == repo);
        if !same_repo || values.iter().all(|v| *v == values[0]) {
            return self.pick(values, &mut |g, v| g.source(v));
        }
        self.use_helper("github");
        let mut opts = Vec::new();
        for k in ["branch", "rootDir"] {
            let column: Vec<Option<&Value>> =
                values.iter().map(|v| v.and_then(|v| v.get(k))).collect();
            if let Some(e) = self.pick_literal(&column) {
                opts.push(format!("{k}: {e}"));
            }
        }
        Some(format!(
            "github({}, {{ {} }})",
            repo.map(|r| self.literal(r)).unwrap_or_default(),
            opts.join(", ")
        ))
    }

    fn pick_command(&mut self, values: &[Option<&Value>]) -> Option<String> {
        self.pick(
            values,
            &mut |g, v| match v.as_array().and_then(|a| command(a)) {
                Some(s) => s,
                None => g.literal(v),
            },
        )
    }

    /// `{ NAME: …, … }` across environments, one per line.
    fn variables(&mut self, values: &[Option<&Value>], at: usize) -> Option<String> {
        let maps: Vec<Option<&Map<String, Value>>> = values
            .iter()
            .map(|v| v.and_then(Value::as_object))
            .collect();
        let names: BTreeSet<&String> = maps.iter().flatten().flat_map(|m| m.keys()).collect();
        if names.is_empty() {
            return None;
        }
        let mut out = String::from("{\n");
        for name in names {
            let column: Vec<Option<&Value>> =
                maps.iter().map(|m| m.and_then(|m| m.get(name))).collect();
            if let Some(e) = self.pick_literal(&column) {
                writeln!(out, "{}{}: {e},", indent(at + 2), key(name)).ok();
            }
        }
        write!(out, "{}}}", indent(at)).ok();
        Some(out)
    }

    /// A service's options object (without `template`), or None if empty.
    fn options(&mut self, values: &[Option<&Map<String, Value>>], at: usize) -> Option<String> {
        let mut lines = String::new();
        let keys = std::iter::once("source")
            .chain(CONFIG_KEYS.iter().copied())
            .chain(OTHER_KEYS.iter().copied());
        for k in keys {
            let column: Vec<Option<&Value>> =
                values.iter().map(|m| m.and_then(|m| m.get(k))).collect();
            let expr = match k {
                "source" => self.pick_source(&column),
                "startCommand" | "preDeployCommand" => self.pick_command(&column),
                "variables" => self.variables(&column, at + 2),
                _ => self.pick_literal(&column),
            };
            if let Some(e) = expr {
                writeln!(lines, "{}{k}: {e},", indent(at + 2)).ok();
            }
        }
        (!lines.is_empty()).then(|| format!("{{\n{lines}{}}}", indent(at)))
    }

    fn body(&mut self, project: &str, roots: &[Value]) -> String {
        let services: Vec<Option<&Map<String, Value>>> = roots
            .iter()
            .map(|r| r.get("services").and_then(Value::as_object))
            .collect();
        let names: BTreeSet<&String> = services.iter().flatten().flat_map(|m| m.keys()).collect();
        let mut out = String::new();
        let mut resources = Vec::new();
        for name in names {
            let column: Vec<Option<&Map<String, Value>>> = services
                .iter()
                .map(|m| m.and_then(|m| m.get(name)).and_then(Value::as_object))
                .collect();
            let template = column
                .iter()
                .flatten()
                .find_map(|m| m.get("template").and_then(Value::as_str))
                .and_then(|t| TEMPLATES.iter().find(|x| **x == t).copied());
            let helper = template.unwrap_or("service");
            self.use_helper(helper);
            let constant = ident(name);
            let call = match self.options(&column, 2) {
                Some(opts) => format!("{helper}({}, {opts})", string(name)),
                None => format!("{helper}({})", string(name)),
            };
            writeln!(out, "  const {constant} = {call};").ok();
            let present: Vec<usize> = (0..column.len()).filter(|i| column[*i].is_some()).collect();
            if present.len() == column.len() {
                resources.push(constant);
            } else {
                let cond = self.condition(&present);
                resources.push(format!("{cond} && {constant}"));
            }
        }
        let shared: Vec<Option<&Value>> = roots
            .iter()
            .map(|r| r.get("shared").and_then(|s| s.get("variables")))
            .collect();
        let shared = self.variables(&shared, 4);
        if !out.is_empty() {
            out.push('\n');
        }
        writeln!(out, "  return project({}, {{", string(project)).ok();
        writeln!(out, "    resources: [{}],", resources.join(", ")).ok();
        if let Some(v) = shared {
            writeln!(out, "    variables: {v},").ok();
        }
        out.push_str("  });\n");
        out
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::iac::model::ConfigFile;
    use crate::iac::pull::environment_spec;
    use crate::iac::pull::tests::env;
    use crate::iac::script::tests::runtime;

    /// `<new dir>/.modula/<name>`.
    pub(crate) fn temp_file(name: &str, text: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("modula-ts-{}-{n}", std::process::id()));
        std::fs::create_dir_all(dir.join(".modula")).unwrap();
        let path = dir.join(".modula").join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn commands_take_their_shortest_form() {
        let argv = |a: &[&str]| a.iter().map(|s| Value::from(*s)).collect::<Vec<_>>();
        assert_eq!(
            command(&argv(&["sh", "apps/api/predeploy.sh"])).as_deref(),
            Some("\"sh apps/api/predeploy.sh\"")
        );
        assert_eq!(
            command(&argv(&["sh", "-c", "bun run migrate && bun start"])).as_deref(), // no-shell: allow (test data)
            Some("\"bun run migrate && bun start\"")
        );
        // An argument with a space can't be written as one string.
        assert_eq!(command(&argv(&["echo", "a b"])), None);
        assert_eq!(command(&argv(&[])), None);
        assert_eq!(ident("anton-api"), "antonApi");
        assert_eq!(ident("postgres"), "postgresService");
    }

    #[test]
    fn generated_files_read_back_per_environment() {
        let envs = [
            env("production", "main", "api.example.com", None),
            env(
                "staging",
                "staging",
                "staging.example.com",
                Some(("DEBUG", "1")),
            ),
        ];
        let pulled = generate("shop", &envs, None).unwrap();
        let text = &pulled.text;
        for secret in ["leak-me-not", "hunter2", "POSTGRES_PASSWORD"] {
            assert!(!text.contains(secret), "{secret} leaked:\n{text}");
        }
        assert!(
            text.contains(r#"source: github("acme/api", { branch: ctx.isEnvironment("staging") ? "staging" : "main" }),"#),
            "{text}"
        );
        assert!(
            text.contains("preDeployCommand: \"sh migrate.sh\","),
            "{text}"
        );
        assert!(text.contains("SEAL: secret({ sealed: true }),"), "{text}");
        assert!(
            text.contains("const postgresService = postgres(\"postgres\", {"),
            "{text}"
        );

        let Some(rt) = runtime() else { return };
        let path = temp_file("modula.ts", text);
        let file = ConfigFile::evaluate(&path, rt).unwrap_or_else(|e| panic!("{e:#}\n{text}"));
        let mut guessed = BTreeSet::new();
        for e in &envs {
            let want = environment_spec(e, "shop", &mut guessed).unwrap();
            let got = file.resolve(Some(&e.name)).unwrap();
            assert_eq!(got, want, "{}:\n{text}", e.name);
        }
        // Without an environment: the default one.
        let want = environment_spec(&envs[0], "shop", &mut guessed).unwrap();
        assert_eq!(file.resolve(None).unwrap(), want, "{text}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn partial_and_per_environment_services() {
        let mut staging = env("staging", "staging", "s.example.com", None);
        staging.services.retain(|s| s.service_name == "api");
        let mut prod = env("production", "main", "a.example.com", None);
        prod.services[0]
            .variables
            .push(pull::tests::var("KEEP", Some("x"), false));
        let envs = [prod, staging];
        let only = BTreeSet::from(["api".to_owned(), "postgres".to_owned()]);
        let pulled = generate("shop", &envs, Some(&only)).unwrap();
        let text = &pulled.text;
        assert!(text.contains("export const partial = true;"), "{text}");
        assert!(
            text.contains(
                r#"resources: [api, ctx.isEnvironment("production") && postgresService],"#
            ),
            "{text}"
        );
        assert!(
            !text.contains("REGION"),
            "shared variables stay out:\n{text}"
        );

        let Some(rt) = runtime() else { return };
        let path = temp_file("modula.ts", text);
        let file = ConfigFile::evaluate(&path, rt).unwrap_or_else(|e| panic!("{e:#}\n{text}"));
        let (specs, _) = pull::environment_specs("shop", &envs, Some(&only)).unwrap();
        for (e, want) in envs.iter().zip(specs) {
            assert_eq!(
                file.resolve(Some(&e.name)).unwrap(),
                want,
                "{}:\n{text}",
                e.name
            );
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }
}
