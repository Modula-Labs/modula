//! `.modula/modula.ts`: config as code in TypeScript. The CLI runs the file
//! with the user's Node (22.18 or newer, which strips types itself) or Bun,
//! once per environment, and reads the project as JSON from stdout.
//!
//! `import … from "modula/iac"` needs nothing installed: the module
//! (`packages/iac/iac.js`) is embedded here, and a prelude maps the import
//! to it, a `module.registerHooks` resolve hook on Node and a `Bun.plugin`
//! on Bun. [`RUNNER`] then imports the file, calls its default export with
//! the context and prints `{ spec, environments, warnings }`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail};
use base64::Engine as _;
use serde::Deserialize;

/// The `modula/iac` module.
pub const MODULE: &str = include_str!("../../../../packages/iac/iac.js");
const RUNNER: &str = include_str!("runner.mjs");

/// The import specifiers that resolve to [`MODULE`]: `modula/iac`, and the
/// npm package's own name (`modula-deploy/iac`).
/// The config file itself always loads as an ES module, even under a
/// `package.json` with `"type": "commonjs"`.
const NODE_PRELUDE: &str = r#"import { registerHooks } from "node:module";
import { pathToFileURL as fileUrl } from "node:url";
const configUrl = fileUrl(process.env.MODULA_IAC_FILE).href;
const configFormat = /\.m?ts$/.test(configUrl) ? "module-typescript" : "module";
registerHooks({
  resolve(specifier, context, next) {
    if (specifier === "modula/iac" || specifier === "modula-deploy/iac") {
      return { url: process.env.MODULA_IAC_MODULE_URL, shortCircuit: true };
    }
    return next(specifier, context);
  },
  load(url, context, next) {
    if (url === configUrl) {
      const loaded = next(url, { ...context, format: configFormat });
      return { ...loaded, format: configFormat };
    }
    return next(url, context);
  },
});
"#;

const BUN_PRELUDE: &str = r#"Bun.plugin({
  name: "modula-iac",
  setup(build) {
    const iac = () => ({ contents: process.env.MODULA_IAC_MODULE_SOURCE, loader: "js" });
    build.module("modula/iac", iac);
    build.module("modula-deploy/iac", iac);
  },
});
"#;

/// Node with type stripping on by default: 22.18+, 23.6+, 24+.
const NODE_MINIMUM: &str = "22.18";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runtime {
    Node(PathBuf),
    Bun(PathBuf),
}

impl Runtime {
    /// `MODULA_JS_RUNTIME` (node, bun or a path to either), else Node when
    /// it is new enough, else Bun.
    pub fn detect() -> anyhow::Result<Self> {
        if let Ok(wanted) = std::env::var("MODULA_JS_RUNTIME")
            && !wanted.is_empty()
        {
            let path = find_program(&wanted).unwrap_or_else(|| PathBuf::from(&wanted));
            let name = path
                .file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            return if name.starts_with("bun") {
                Ok(Self::Bun(path))
            } else {
                check_node(&path)?;
                Ok(Self::Node(path))
            };
        }
        let node = find_program("node");
        let node_problem = match &node {
            Some(path) => match check_node(path) {
                Ok(()) => return Ok(Self::Node(path.clone())),
                Err(e) => Some(e),
            },
            None => None,
        };
        if let Some(bun) = find_program("bun") {
            return Ok(Self::Bun(bun));
        }
        match node_problem {
            Some(e) => Err(e),
            None => bail!(
                "modula.ts needs Node.js {NODE_MINIMUM} or newer, or Bun, on your PATH (neither was found); \
                 or use .modula/modula.toml"
            ),
        }
    }

    fn command(&self) -> Command {
        match self {
            Self::Node(path) => {
                let mut c = Command::new(path);
                c.args([
                    "--disable-warning=ExperimentalWarning",
                    "--input-type=module",
                    "-e",
                ])
                .arg(format!("{NODE_PRELUDE}{RUNNER}"))
                .env(
                    "MODULA_IAC_MODULE_URL",
                    format!(
                        "data:text/javascript;base64,{}",
                        base64::engine::general_purpose::STANDARD.encode(MODULE)
                    ),
                );
                c
            }
            Self::Bun(path) => {
                let mut c = Command::new(path);
                c.arg("-e")
                    .arg(format!("{BUN_PRELUDE}{RUNNER}"))
                    .env("MODULA_IAC_MODULE_SOURCE", MODULE);
                c
            }
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Node(_) => "node",
            Self::Bun(_) => "bun",
        }
    }
}

/// `program` on PATH (or as given, when it is a path).
fn find_program(program: &str) -> Option<PathBuf> {
    let p = Path::new(program);
    if p.components().count() > 1 {
        return p.is_file().then(|| p.to_owned());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| {
            let plain = dir.join(program);
            let exe = dir.join(format!("{program}.exe"));
            [plain, exe]
        })
        .find(|c| c.is_file())
}

fn check_node(path: &Path) -> anyhow::Result<()> {
    let out = Command::new(path)
        .arg("--version")
        .output()
        .with_context(|| format!("cannot run {}", path.display()))?;
    let version = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if node_supported(&version) {
        Ok(())
    } else {
        bail!(
            "modula.ts needs Node.js {NODE_MINIMUM} or newer (it runs TypeScript itself); {} is {version}. \
             Upgrade Node, install Bun, or use .modula/modula.toml",
            path.display()
        )
    }
}

/// Whether `v22.18.0` (as `node --version` prints it) strips types by default.
fn node_supported(version: &str) -> bool {
    let mut parts = version.trim_start_matches('v').split('.');
    let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    match major {
        22 => minor >= 18,
        23 => minor >= 6,
        m => m >= 24,
    }
}

/// What one evaluation printed.
#[derive(Debug, Clone, Deserialize)]
pub struct Evaluated {
    /// The project, in the shape of modula.toml (no `environments`).
    pub spec: serde_json::Value,
    /// `export const environments = [...]`: environments the file knows.
    #[serde(default)]
    pub environments: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Run `file` for `environment` (None: the base, as `config validate`
/// sees it). The working directory is the repository root (`root`).
pub fn evaluate(
    runtime: &Runtime,
    file: &Path,
    root: &Path,
    environment: Option<&str>,
    preview: bool,
) -> anyhow::Result<Evaluated> {
    // The real path, as the runtime's module URLs have it (see NODE_PRELUDE).
    let file =
        std::fs::canonicalize(file).with_context(|| format!("cannot read {}", file.display()))?;
    let mut cmd = runtime.command();
    cmd.current_dir(root)
        .env("MODULA_IAC_FILE", &file)
        .env("MODULA_IAC_PREVIEW", if preview { "1" } else { "0" })
        .env_remove("MODULA_IAC_ENVIRONMENT");
    if let Some(env) = environment {
        cmd.env("MODULA_IAC_ENVIRONMENT", env);
    }
    let out = cmd
        .output()
        .with_context(|| format!("cannot run {}", runtime.name()))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let label = match environment {
        Some(env) => format!("{} (environment {env})", file.display()),
        None => file.display().to_string(),
    };
    if !out.status.success() {
        let detail = stderr.trim();
        bail!(
            "{label} failed under {}{}",
            runtime.name(),
            if detail.is_empty() {
                String::new()
            } else {
                format!(":\n{detail}")
            }
        );
    }
    // User logging goes to stderr; pass it on.
    if !stderr.trim().is_empty() {
        eprint!("{stderr}");
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let last = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    serde_json::from_str(last)
        .with_context(|| format!("{label}: {} printed no result", runtime.name()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn node_versions() {
        assert!(node_supported("v22.18.0"));
        assert!(node_supported("v22.19.1"));
        assert!(!node_supported("v22.17.9"));
        assert!(!node_supported("v20.10.0"));
        assert!(!node_supported("v23.5.0"));
        assert!(node_supported("v23.6.0"));
        assert!(node_supported("v24.0.0"));
        assert!(!node_supported("garbage"));
    }

    /// A runtime for tests, or None (with a note) when this machine has none.
    pub(crate) fn runtime() -> Option<Runtime> {
        match Runtime::detect() {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("skipping modula.ts evaluation tests: {e:#}");
                None
            }
        }
    }

    use crate::iac::model::{ConfigFile, RepoSource, SourceSpec, VarSpec};
    use crate::iac::typescript::tests::temp_file;

    /// The user's real file, adapted from Railway's railway.ts.
    const ANTON: &str = r#"
import { defineModula, github, preserve, project, service } from "modula/iac";
export const partial = "anton-api";
export default defineModula((ctx) => {
  console.log("logging is fine");
  const api = service("Anton API", {
    source: github("the-anton-project/anton-video-new", { branch: ctx.isEnvironment("production") ? "main" : "staging" }),
    preDeploy: "sh apps/api/scripts/predeploy.sh",
    healthcheck: "/v1/ready", healthcheckTimeout: 120, replicas: 1,
    deploy: { restartPolicyMaxRetries: 5 },
    variables: { DATABASE_URL: preserve(), ANTON_MIN_AI_MACOS_VERSION: "latest", RAILWAY_DEPLOYMENT_DRAINING_SECONDS: "30" },
  });
  const typed: number = 1;
  return project("anton", { resources: [api] });
});
"#;

    fn check_anton(rt: Runtime) {
        let path = temp_file("modula.ts", ANTON);
        let file = ConfigFile::evaluate(&path, rt).unwrap_or_else(|e| panic!("{e:#}"));
        assert!(file.is_script());
        assert_eq!(file.project(), Some("anton"));
        assert_eq!(file.dir(), path.parent().unwrap().parent().unwrap());
        for (env, branch) in [
            ("production", "main"),
            ("staging", "staging"),
            ("Production", "main"),
        ] {
            let spec = file.resolve(Some(env)).unwrap();
            assert!(spec.partial);
            let api = &spec.services["anton-api"];
            assert_eq!(
                api.source,
                Some(SourceSpec::Repo(RepoSource {
                    repo: "the-anton-project/anton-video-new".into(),
                    branch: branch.into(),
                    root_dir: None,
                })),
                "{env}"
            );
            assert_eq!(
                api.pre_deploy_command.as_deref(),
                Some(&["sh".to_owned(), "apps/api/scripts/predeploy.sh".to_owned()][..])
            );
            assert_eq!(api.healthcheck_path.as_deref(), Some("/v1/ready"));
            assert_eq!(api.healthcheck_timeout_secs, Some(120));
            assert_eq!(api.replicas, Some(1));
            assert_eq!(api.restart_policy_max_retries, Some(5));
            assert_eq!(api.preserve, ["DATABASE_URL"]);
            assert_eq!(
                api.variables["ANTON_MIN_AI_MACOS_VERSION"],
                VarSpec::Value("latest".into())
            );
            assert!(!api.variables.contains_key("DATABASE_URL"));
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn the_railway_style_example_maps_to_the_model() {
        let Some(rt) = runtime() else { return };
        check_anton(rt);
        // The output also carries the warnings.
        let path = temp_file("modula.ts", ANTON);
        let out = evaluate(
            &runtime().unwrap(),
            &path,
            path.parent().unwrap(),
            Some("production"),
            false,
        )
        .unwrap();
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("RAILWAY_DEPLOYMENT_DRAINING_SECONDS")
                    && w.contains("drainingSeconds")),
            "{:?}",
            out.warnings
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn bun_runs_it_too() {
        let Some(bun) = find_program("bun") else {
            eprintln!("skipping: bun isn't on PATH");
            return;
        };
        check_anton(Runtime::Bun(bun));
    }

    #[test]
    fn helpers_cover_templates_secrets_refs_and_previews() {
        let Some(rt) = runtime() else { return };
        let path = temp_file(
            "modula.ts",
            r#"
import { defineModula, image, postgres, project, ref, secret, service, local, redis } from "modula/iac";
export const environments = ["staging"];
export default defineModula(async (ctx) => project("shop", {
  resources: [
    service("Web", {
      source: ctx.isPreview ? image("nginx:preview") : local(),
      start: "node server.js",
      build: { builder: "DOCKERFILE", watchPatterns: ["src/**"] },
      deploy: { restartPolicyType: "ALWAYS", numReplicas: 2, sleepApplication: true, startCommand: undefined },
      variables: { DB: ref("Main DB", "DATABASE_URL") + "?pool=5", KEY: secret(), VAULT: secret({ sealed: true }), N: 4, SKIP: null },
      domains: ["shop.example.com", { host: "admin.example.com", port: 9000 }],
    }),
    postgres("Main DB", { memory: 512 }),
    ctx.isEnvironment("staging") && redis(),
  ],
  variables: { REGION: ref("shared", "X") },
}));
"#,
        );
        let file = ConfigFile::evaluate(&path, rt).unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(file.environments.keys().collect::<Vec<_>>(), ["staging"]);
        let base = file.resolve(None).unwrap();
        let web = &base.services["web"];
        assert_eq!(
            web.start_command.as_deref(),
            Some(&["node".to_owned(), "server.js".to_owned()][..])
        );
        assert_eq!(web.builder, Some(crate::iac::model::Builder::Dockerfile));
        assert_eq!(
            web.restart_policy,
            Some(crate::iac::model::RestartPolicy::Always)
        );
        assert_eq!(web.replicas, Some(2));
        assert!(web.serverless.is_some());
        assert_eq!(web.watch_paths.as_deref(), Some(&["src/**".to_owned()][..]));
        assert!(web.is_local());
        assert_eq!(
            web.variables["DB"],
            VarSpec::Value("${{main-db.DATABASE_URL}}?pool=5".into())
        );
        assert_eq!(web.variables["KEY"], VarSpec::secret(false));
        assert_eq!(web.variables["VAULT"], VarSpec::secret(true));
        assert_eq!(web.variables["N"], VarSpec::Integer(4));
        assert!(!web.variables.contains_key("SKIP"));
        assert_eq!(web.domains.as_ref().unwrap()[1].port(), Some(9000));
        assert_eq!(
            base.services["main-db"].template.as_deref(),
            Some("postgres")
        );
        assert_eq!(base.services["main-db"].memory_mb, Some(512));
        assert!(!base.services.contains_key("redis"));
        assert_eq!(
            base.shared.variables["REGION"],
            VarSpec::Value("${{shared.X}}".into())
        );
        assert!(
            file.resolve(Some("staging"))
                .unwrap()
                .services
                .contains_key("redis")
        );
        let preview = file.resolve_for(Some("pr-1"), true).unwrap();
        assert!(matches!(
            preview.services["web"].source,
            Some(SourceSpec::Image(_))
        ));
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn errors_name_the_file_and_the_cause() {
        let Some(rt) = runtime() else { return };
        let err = |text: &str| {
            let path = temp_file("modula.ts", text);
            let e = format!("{:#}", ConfigFile::evaluate(&path, rt.clone()).unwrap_err());
            let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
            e
        };
        let e = err("export default () => { throw new Error(\"boom here\"); };\n");
        assert!(
            e.contains("modula.ts threw") && e.contains("boom here"),
            "{e}"
        );
        let e = err("export default defineModula((\n");
        assert!(e.contains("cannot load") && e.contains("modula.ts"), "{e}");
        let e = err("export const x = 1;\n");
        assert!(e.contains("no default export"), "{e}");
        let e = err(
            "import { project } from \"modula/iac\";\nexport default project(\"p\", { resources: [{}] });\n",
        );
        assert!(e.contains("resources are service()"), "{e}");
        // The model still validates what comes out.
        let e = err(
            "import { project, service, image } from \"modula/iac\";\nexport default project(\"p\", { resources: [service(\"api\", { source: image(\"x\"), prot: 1 })] });\n",
        );
        assert!(e.contains("unknown key `prot`"), "{e}");
        // A plain object works as the default export.
        let path = temp_file(
            "modula.ts",
            "import { project, service, image } from \"modula/iac\";\nexport default project(\"p\", { resources: [service(\"api\", { source: image(\"nginx\") })] });\n",
        );
        let file = ConfigFile::evaluate(&path, rt.clone()).unwrap();
        assert!(
            file.resolve(Some("production"))
                .unwrap()
                .services
                .contains_key("api")
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn a_commonjs_package_json_doesnt_break_it() {
        let Some(rt) = runtime() else { return };
        let path = temp_file("modula.ts", ANTON);
        let root = path.parent().unwrap().parent().unwrap().to_owned();
        std::fs::write(root.join("package.json"), "{ \"type\": \"commonjs\" }").unwrap();
        let r = ConfigFile::evaluate(&path, rt);
        let _ = std::fs::remove_dir_all(&root);
        r.unwrap_or_else(|e| panic!("{e:#}"));
    }

    #[test]
    fn config_files_are_found_in_dot_modula() {
        let path = temp_file("modula.toml", "project = \"p\"\n");
        let root = path.parent().unwrap().parent().unwrap().to_owned();
        let sub = root.join("apps/api");
        std::fs::create_dir_all(&sub).unwrap();
        // Root-level files aren't config files.
        std::fs::write(sub.join("modula.toml"), "[build]\n").unwrap();
        assert_eq!(ConfigFile::find(&sub).unwrap(), Some(path.clone()));
        let file = ConfigFile::load(&path).unwrap();
        assert_eq!(file.dir(), root);
        std::fs::write(root.join(".modula/modula.ts"), "").unwrap();
        let e = format!("{:#}", ConfigFile::find(&sub).unwrap_err());
        assert!(
            e.contains("several config files (modula.ts, modula.toml)"),
            "{e}"
        );
        assert_eq!(
            ConfigFile::find(&std::env::temp_dir().join("nowhere-modula")).unwrap(),
            None
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
