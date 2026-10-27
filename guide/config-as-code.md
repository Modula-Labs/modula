# Config as code: `.modula/modula.ts`

Describe a whole project in one file in your repository, review changes in
pull requests, and apply them with the CLI:

```bash
modula init             # link the directory and write .modula/modula.ts
modula config init      # (re)write it from the linked project (alias: config pull)
modula config plan      # what would change (like terraform plan)
modula config apply     # make it so (asks first; --yes for CI)
```

The file describes services, their settings, variables, domains, volumes and
TCP proxies, with per-environment differences. Secret values never go in it.
Write it in TypeScript (`modula.ts`, like Railway's `railway.ts`) or TOML
(`modula.toml`); both describe the same thing.

This is different from the build-time `[build]`/`[deploy]` file the builder
reads at the repository root (see [projects and services → config as code](projects-and-services.md#config-as-code)).

## The `.modula/` directory

```
.modula/
  modula.ts            # the config: commit it (or modula.toml / modula.json)
  .gitignore           # written by the CLI: keeps the next three out of git
  project.toml         # the directory link (modula init / modula link)
  token                # optional per-directory login (modula login --local)
  secrets.<env>.env    # secret values for apply (dotenv)
```

The CLI uses the config file in the nearest `.modula/` in the current
directory or a parent; a `.modula/` may hold only one of `modula.ts`,
`modula.toml` and `modula.json`. `--file PATH` picks another file.

## TypeScript: `modula.ts`

```ts
import { defineModula, github, postgres, preserve, project, ref, secret, service } from "@modula-labs/iac";

// This repository describes only some of the project's services.
export const partial = "anton-api";

export default defineModula((ctx) => {
  const db = postgres("postgres", { memory: 1024 });
  const api = service("Anton API", {                     // → anton-api
    source: github("acme/anton", { branch: ctx.isEnvironment("production") ? "main" : "staging" }),
    preDeploy: "sh apps/api/scripts/predeploy.sh",
    healthcheck: "/v1/ready",
    healthcheckTimeout: 120,
    replicas: 1,
    deploy: { restartPolicyMaxRetries: 5 },
    drainingSeconds: 30,
    domains: ctx.isEnvironment("production") ? ["api.example.com"] : [],
    variables: {
      DATABASE_URL: ref("postgres", "DATABASE_URL"),    // ${{postgres.DATABASE_URL}}
      LEGACY_FLAG: preserve(),                         // on the server only; never deleted
      BETTER_AUTH_SECRET: secret(),                    // value from .modula/secrets.<env>.env
      STRIPE_KEY: secret({ sealed: true }),
      NODE_ENV: ctx.isPreview ? "preview" : "production",
    },
  });
  return project("anton", { resources: [api, db], variables: { REGION: "eu" } });
});
```

**Running it.** The CLI runs the file with your Node.js (22.18 or newer,
which runs TypeScript itself) or, if Node is missing or older, Bun
(`MODULA_JS_RUNTIME=node|bun|PATH` picks one). It runs it once per
environment it plans, and once without an environment for `config validate`
and to read `project`. Nothing needs installing: the CLI has `@modula-labs/iac`
built in and resolves the import itself. The file is loaded as an ES module
even under a `"type": "commonjs"` package.json, runs with the repository
root as its working directory, and can import your own modules and read
`process.env`. `console.log` output goes to stderr. When it throws, the
command fails with the error and its stack.

**The context** passed to the function:

| | |
|---|---|
| `ctx.environment` | The environment's name, lowercase (`undefined` without one). |
| `ctx.isEnvironment(...names)` | Whether it is one of `names` (case-insensitive). |
| `ctx.isPreview` | Whether it is a pull request preview environment. |

`export const environments = ["staging"]` names environments that
`config plan --all-envs` should create if missing (a TOML file does that
with `[environments.staging]`). `export const partial = true` (or any
string, as in Railway) is `partial = true`.

**Helpers** (`@modula-labs/iac`):

| Helper | Gives |
|---|---|
| `project(name, { resources, variables, preserve, partial })` | The project; `resources` are services (`false`/`null` entries are skipped, so `ctx.isEnvironment("x") && svc` works). |
| `service(name, options)` | A service. Names become slugs: `"Anton API"` → `anton-api`. |
| `postgres(name?, options?)`, `mysql`, `redis`, `mongo`, `bucket`, `r2`, `database(template, name?, options?)` | A template service (`template = …`). |
| `github("owner/name", { branch, rootDir })`, `image("ref")`, `local(".")` | A `source`. |
| `secret()`, `secret({ sealed: true })` | `{ secret = true }` / sealed. |
| `preserve()` | The variable stays as it is on the server (added to `preserve`). |
| `ref("Service", "VAR")` | `"${{service.VAR}}"` (`ref("shared", "X")` for shared ones). |

Service options are the settings and keys of the TOML file below, plus
Railway's names: `start`/`startCommand`, `preDeploy`/`preDeployCommand`,
`healthcheck`, `healthcheckTimeout` (seconds), `numReplicas`,
`restartPolicyType` (`ON_FAILURE` is accepted), `watchPatterns`, `cron`,
`cronTimeout`, `memory` (MB), `cpu` (millicores), `sleepApplication`
(→ `serverless: {}`), and `deploy: {…}`/`build: {…}` groups whose keys merge
into the service. A command string without shell syntax is split on spaces
(`"sh migrate.sh"` → `["sh", "migrate.sh"]`); one with quotes, `$`, `&&`,
pipes and the like runs as `["sh", "-c", "…"]`; an array is used as is.
Values that are `undefined` or `null` are left out, so
`ctx.isEnvironment("staging") ? 2 : undefined` sets a value in one
environment only. What comes out is checked exactly like a TOML file
(unknown keys, names, sources), and `RAILWAY_*` variables get a warning
(`RAILWAY_DEPLOYMENT_DRAINING_SECONDS` → `drainingSeconds`).

**Editor types.** `npm i -D @modula-labs/iac` (or `bun add -d
@modula-labs/iac`) gives your editor autocomplete and type checking; the
CLI never uses it.

## TOML: `modula.toml`

```toml
project = "anton"                     # name or id (-p overrides it)

[services.api]
source = { repo = "acme/anton", branch = "main" }   # or { image = "…" }, or { local = "." }
builder = "dockerfile"
dockerfilePath = "Dockerfile"
preDeployCommand = ["sh", "apps/api/scripts/predeploy.sh"]
healthcheckPath = "/v1/ready"
healthcheckTimeoutSecs = 120
drainingSeconds = 30
restartPolicy = "on_failure"
restartPolicyMaxRetries = 5
domains = ["api.example.com", { host = "admin.example.com", port = 9000 }]
generateDomain = true                 # keep a generated *.apps domain
preserve = ["LEGACY_FLAG"]            # remote-only variables apply must not delete

[services.api.variables]
NODE_ENV = "production"
WORKERS = 4                           # numbers and booleans become strings
DATABASE_URL = "${{postgres.DATABASE_URL}}?schema=public"
BETTER_AUTH_SECRET = { secret = true }             # value never in git
STRIPE_KEY = { secret = true, sealed = true }      # and stored sealed

[services.postgres]
template = "postgres"                 # postgres, mysql, redis or mongo
memoryMb = 1024

[shared.variables]                    # ${{shared.NAME}}
REGION = "eu"

[environments.staging]                # deep-merged over everything above
services.api.source = { repo = "acme/anton", branch = "staging" }
services.api.domains = []
services.api.variables.BETTER_AUTH_URL = "https://staging.example.com"
```

`.modula/modula.json` with the same structure is accepted too. The keys
below are also the keys of a `modula.ts` service's options and of
`project()`'s output.

### Services

Each `[services.<name>]` is one service (names: lowercase letters, digits,
`-`). It needs a `source` or a `template`:

| `source` | Meaning |
|---|---|
| `{ repo = "owner/name", branch = "main", rootDir = "apps/api" }` | GitHub repository (`branch` defaults to `main`; `rootDir` optional). |
| `{ image = "ghcr.io/acme/api:1.2" }` | Prebuilt image. |
| `{ local = "." }` | A directory you deploy with `modula up`. Config never touches the service's configured source, and can't create such a service: create it once (`modula add` or the dashboard), then config manages its settings. |

The settings keys are exactly the service settings of the API
(`ServiceConfig`, camelCase): `startCommand`, `port`, `healthcheckPath`,
`healthcheckTimeoutSecs`, `restartPolicy`, `restartPolicyMaxRetries`,
`preDeployCommand`, `drainingSeconds`, `memoryMb`, `cpuMillis`, `replicas`,
`builder`, `dockerfilePath`, `buildCommand`, `autoDeploy`, `watchPaths`. Types
and limits: [settings reference](projects-and-services.md#settings-reference).
Unknown keys are errors (`healthcheck_path` gets "did you mean
`healthcheckPath`?").

**A setting left out takes its default** (for a template service, the
template's), so deleting a line resets that setting. `config init` writes
every non-default setting, so a pulled file changes nothing.

Besides settings, a service can have:

| Key | Meaning |
|---|---|
| `domains` | Custom domains: `"host"` or `{ host = "…", port = 3000 }`. Ones not listed are deleted. Generated domains are never deleted. |
| `generateDomain = true` | Make sure there is a generated domain. |
| `volume = "/data"` | The volume's mount path. Template services keep their template's volume when this is left out. |
| `tcpProxy = 5432` | Expose that port through the TCP proxy (public!). Left out: no proxy. |
| `variables` | See below. |
| `preserve = [...]` | Variables that exist only on the server and must not be deleted. |

### Variables

A variable is a string (references like `${{postgres.DATABASE_URL}}` are
kept as written), a number or a boolean, or a **secret**:

- `{ secret = true }` (`secret()` in TypeScript): the value stays out of the
  file. `apply` takes it from `.modula/secrets.<environment>.env` (dotenv lines,
  `KEY=value`), from `--secrets-file PATH`, or with `--from-env` from an
  environment variable of the same name (which wins). If there's no local
  value, the variable must already exist on the server; it is then left as
  is. Names in the secrets file are flat: a secret called `API_KEY` in two
  services gets the same value.
- `{ secret = true, sealed = true }`: also stored [sealed](variables.md)
  (write-only forever). Sealed values can't be compared, so `apply` only sets
  them when they're missing, or always with `--update-sealed`.

Variables on the server that aren't in the file are **deleted**, except the
ones in `preserve` and those a template created (its passwords and
connection URLs), which are managed by the template unless you list them.

### Environments

`[environments.<name>]` holds overrides for one environment and may only
contain `services` and `shared`. It is deep-merged over the base: tables
merge key by key, everything else replaces (`domains = []` clears the list;
a `source` naming `repo`/`image`/`local` replaces the base's source, while
`source.branch = "dev"` alone just changes the branch). Environments without
a section get the base as is.

A new service is added to every environment of a project, so the base lists
all of them. (A service deleted from one environment is still in the others;
re-adding it to that environment from the file isn't supported yet.)

## Commands

All take `--file PATH`; `plan` and `apply` also take `-p`, `-e`,
`--all-envs`, `--secrets-file PATH`, `--from-env` and `--update-sealed`.

### `config init` (alias `config pull`)

Writes `.modula/modula.ts` (`--format toml`: `.modula/modula.toml`; or
`--file PATH`, whose extension picks the format) in the linked directory
from the project (`-p` or the linked one), covering all environments. The base comes
from `production`. In TOML what differs goes in `[environments.*]`; in
TypeScript it becomes `ctx.isEnvironment("staging") ? … : …`, and a service
that exists in only some environments is listed as
`ctx.isEnvironment(…) && svc`. Refuses to overwrite without `--force`, which
also replaces a config file in the other format. `--only SERVICE`
(repeatable) writes just those services as a `partial` file, without shared
variables. Never writes a secret value: sealed variables become
`{ secret = true, sealed = true }` (`secret({ sealed: true })`), variables
whose names look like credentials (`PASSWORD`, `SECRET`, `TOKEN`, `KEY`,
`PRIVATE`, `CREDENTIAL`) become secrets (listed on stderr), and template
variables are left out. A written `modula.ts` is run once to check it,
when Node or Bun is available.

### `config plan [-e ENV | --all-envs] [--json]`

Compares the file with the server for the environment (`-e`, else the
linked one, else `production`) or every environment (the project's and the
file's), and prints the changes:

```
Environment anton/production
  ~ service api
      ~ port: 8080 → 3000
      + variable BETTER_AUTH_SECRET (secret)
      ~ variable NODE_ENV: "staging" → "production"
      - variable OLD_FLAG
      + domain api.example.com
  ! manual: worker: not in the file; delete it in the dashboard …
Plan: 2 to add, 2 to change, 1 to delete.
```

Values are shown for plain variables only; secrets show their name. `--json`
prints `{ project, changes, environments: [...] }` with each environment's
services (`action`, the desired `config`), settings (`from`/`to`), variables (`op`, `secret`, `sealed`, and
`from`/`to` for non-secrets), domains, volumes, TCP proxies, `manual` steps,
`warnings` and `errors`. Exits 1 when there are errors (e.g. a secret with no
value anywhere).

### `config apply [-e ENV | --all-envs] [--yes] [--deploy]`

Prints the plan, asks (or needs `--yes` without a terminal), then applies it
through the regular API, in order: missing environments, new services, then
per environment settings, volumes and TCP proxies, variables (shared first),
domains (deletes before adds, so a host can move between services). Nothing
is applied when the plan has errors.

Changes stay pending like any edit; `--deploy` deploys the changed services
afterwards (every service with pending changes when shared variables
changed).

Apply isn't one transaction: if a step fails (say, a generated domain on an
instance without an apps domain), what ran before it stays applied. Fix the
cause and run `plan`/`apply` again; it picks up where it stopped.

A new service is created in every environment with the planned
environment's settings; run `apply --all-envs` (or apply each environment) so
the others get theirs.

**What apply doesn't do** (listed as `manual` in the plan): deleting a
service or a volume, and moving a volume to another path, need a
re-authenticated dashboard session, which API tokens never have. Services
with a `local` source must be created first. Environments are never deleted.

### `config validate`

Parses the file and resolves every environment without contacting the
server: unknown keys, bad names, invalid sources, secrets without
`secret = true`. A `modula.ts` is run without an environment and for each
name in its `environments` export.

## Secrets, tokens and git

- Commit the config (`.modula/modula.ts`). The rest of `.modula/` stays out
  of git: whenever the CLI writes into `.modula/` it makes sure
  `.modula/.gitignore` lists `project.toml` (the directory link), `token`
  (a per-directory login) and `secrets.*` (secret values), creating it or
  adding the missing lines, and says so on stderr. `config apply` does the
  same when it reads a secrets file; `config plan` only warns. When the
  repository's own `.gitignore` ignores all of `.modula/`, writing a config
  warns that it won't be committed.
- Reading the environment for `plan` and `pull` uses
  `GET /api/environments/{id}/config` (Member role). It returns the values of
  non-sealed variables (so plans can show value changes), never sealed ones,
  and every call is in the audit log (`environment.config_read`) with the
  names it returned.
- Tokens for CI: `MODULA_URL` + `MODULA_TOKEN`, or a per-directory login
  with `modula login --local` (see [CLI → authentication](cli.md#authentication)).

## CI example

```yaml
- run: modula config plan --all-envs
- run: modula config apply -e production --yes --deploy --from-env
  env:
    MODULA_URL: https://deploy.example.com
    MODULA_TOKEN: ${{ secrets.MODULA_TOKEN }}
    BETTER_AUTH_SECRET: ${{ secrets.BETTER_AUTH_SECRET }}
```

## From Railway

**`railway.ts` → `modula.ts`.** Railway's `.railway/railway.ts` describes a
whole project; `.modula/modula.ts` is the same idea with the same shape:
change the import to `"@modula-labs/iac"`, move the file to `.modula/modula.ts`,
and check the names below. `railway init` / `railway config plan` /
`railway config apply` are `modula init` / `modula config plan` /
`modula config apply` (unlisted variables are deleted unless preserved, as
there). Service names become slugs (`"Anton API"` → `anton-api`), so a
`partial` file keeps Railway's string as is but services are matched by
slug.

Railway's `railway.toml`/`railway.json` describe one service's build and
deploy settings; they keep working at build time. To manage the whole
project, move their keys into the service:

| Railway | `modula.ts` (service options) | `modula.toml` (in `[services.<name>]`) |
|---|---|---|
| `import … from "railway/iac"` | `import … from "@modula-labs/iac"` | |
| `.railway/railway.ts` | `.modula/modula.ts` | `.modula/modula.toml` |
| `export const partial = "…"` | the same | `partial = true` |
| `build.builder` (`DOCKERFILE`, `RAILPACK`) | `builder: "DOCKERFILE"` or `build: { builder }` | `builder = "dockerfile"` / `"railpack"` |
| `build.builder = "NIXPACKS"` | use `"RAILPACK"` | `builder = "railpack"` |
| `build.buildCommand` | `buildCommand` | `buildCommand` |
| `build.dockerfilePath` | `dockerfilePath` | `dockerfilePath` |
| `build.watchPatterns` | `watchPatterns` or `watchPaths` | `watchPaths` |
| `deploy.startCommand` (shell string) | `startCommand: "…"` or `start` | `startCommand = ["sh", "-c", "…"]` or an argv list |
| `deploy.preDeployCommand` | `preDeployCommand: "…"` or `preDeploy` | `preDeployCommand` (argv list) |
| `deploy.healthcheckPath` / `healthcheckTimeout` | `healthcheck` / `healthcheckTimeout` | `healthcheckPath` / `healthcheckTimeoutSecs` |
| `deploy.restartPolicyType` / `restartPolicyMaxRetries` | the same (`deploy: { … }` works) | `restartPolicy = "on_failure"` (lowercase) / `restartPolicyMaxRetries` |
| `deploy.numReplicas` | `numReplicas` or `replicas` | `replicas` |
| `deploy.sleepApplication` | the same | `serverless = {}` |
| `deploy.overlapSeconds` | ignored (warning) | |
| `RAILWAY_DEPLOYMENT_DRAINING_SECONDS` variable | `drainingSeconds` | `drainingSeconds` |
| `environments.<env>.…` | `ctx.isEnvironment("<env>") ? … : …` | `[environments.<env>] services.<name>.…` |
