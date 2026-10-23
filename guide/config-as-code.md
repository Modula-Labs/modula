# Config as code: `modula.toml`

Describe a whole project in one file at the root of your repository, review
changes in pull requests, and apply them with the CLI:

```bash
modula-deploy config init      # write modula.toml from the linked project (alias: config pull)
modula-deploy config plan      # what would change (like terraform plan)
modula-deploy config apply     # make it so (asks first; --yes for CI)
```

The file describes services, their settings, variables, domains, volumes and
TCP proxies, with per-environment overrides. Secret values never go in it.

This is different from the build-time `[build]`/`[deploy]` file the builder
reads (see [projects and services → config as code](projects-and-services.md#config-as-code)).
A `modula.toml` with `project` or `services` and no `[build]`/`[deploy]` is
this kind of file, and the builder skips it, so a `railway.toml` next to it
still applies to builds.

## The file

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

`modula.json` with the same structure is accepted too (`modula.toml` wins if
both exist). `--file PATH` picks another file; by default the CLI uses the
nearest one in the current directory or a parent.

### Services

Each `[services.<name>]` is one service (names: lowercase letters, digits,
`-`). It needs a `source` or a `template`:

| `source` | Meaning |
|---|---|
| `{ repo = "owner/name", branch = "main", rootDir = "apps/api" }` | GitHub repository (`branch` defaults to `main`; `rootDir` optional). |
| `{ image = "ghcr.io/acme/api:1.2" }` | Prebuilt image. |
| `{ local = "." }` | A directory you deploy with `modula-deploy up`. Config never touches the service's configured source, and can't create such a service: create it once (`modula-deploy add` or the dashboard), then config manages its settings. |

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

- `{ secret = true }`: the value stays out of the file. `apply` takes it from
  `.modula/secrets.<environment>.env` next to `modula.toml` (dotenv lines,
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

Every service exists in every environment of a project, so the base lists
all of them.

## Commands

All take `--file PATH`; `plan` and `apply` also take `-p`, `-e`,
`--all-envs`, `--secrets-file PATH`, `--from-env` and `--update-sealed`.

### `config init` (alias `config pull`)

Writes `modula.toml` (or `--file`) from the project (`-p` or the linked one):
all environments, the base from `production`, what differs in
`[environments.*]`. Refuses to overwrite without `--force`. Never writes a
secret value: sealed variables become `{ secret = true, sealed = true }`,
variables whose names look like credentials (`PASSWORD`, `SECRET`, `TOKEN`,
`KEY`, `PRIVATE`, `CREDENTIAL`) become `{ secret = true }` (listed on
stderr), and template variables are left out.

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
`secret = true`.

## Secrets, tokens and git

- Commit `modula.toml`. Keep `.modula/` out of git: it holds the directory
  link (`project.toml`), an optional per-directory token (`token`) and
  secrets files (`secrets.<env>.env`). Whenever the CLI writes into
  `.modula/` it makes sure the repository's `.gitignore` (at the git root;
  created if missing) ignores `.modula/`, appending an annotated block and
  saying so on stderr. If your `.gitignore` already names `.modula/token`
  and `.modula/secrets.*` (because you chose to commit `project.toml`), it is
  left alone. `config apply` does the same when it reads a secrets file;
  `config plan` only warns.
- Reading the environment for `plan` and `pull` uses
  `GET /api/environments/{id}/config` (Member role). It returns the values of
  non-sealed variables (so plans can show value changes), never sealed ones,
  and every call is in the audit log (`environment.config_read`) with the
  names it returned.
- Tokens for CI: `MODULA_URL` + `MODULA_TOKEN`, or a per-directory login
  with `modula-deploy login --local` (see [CLI → authentication](cli.md#authentication)).

## CI example

```yaml
- run: modula-deploy config plan --all-envs
- run: modula-deploy config apply -e production --yes --deploy --from-env
  env:
    MODULA_URL: https://deploy.example.com
    MODULA_TOKEN: ${{ secrets.MODULA_TOKEN }}
    BETTER_AUTH_SECRET: ${{ secrets.BETTER_AUTH_SECRET }}
```

## From Railway

Railway's `railway.toml`/`railway.json` describe one service's build and
deploy settings; they keep working at build time. To manage the whole
project, move their keys into the service's table here:

| Railway | `modula.toml` (in `[services.<name>]`) |
|---|---|
| `build.builder` (`DOCKERFILE`, `RAILPACK`, `NIXPACKS`) | `builder = "dockerfile"` / `"railpack"` |
| `build.buildCommand` | `buildCommand` |
| `build.dockerfilePath` | `dockerfilePath` |
| `build.watchPatterns` | `watchPaths` |
| `deploy.startCommand` (shell string) | `startCommand = ["sh", "-c", "…"]` or an argv list |
| `deploy.preDeployCommand` | `preDeployCommand` (argv list) |
| `deploy.healthcheckPath` / `healthcheckTimeout` | `healthcheckPath` / `healthcheckTimeoutSecs` |
| `deploy.restartPolicyType` / `restartPolicyMaxRetries` | `restartPolicy = "on_failure"` (lowercase) / `restartPolicyMaxRetries` |
| `deploy.numReplicas` | `replicas` (1 for now) |
| `environments.<env>.…` | `[environments.<env>] services.<name>.…` |

Railway's `.railway/railway.ts` maps to the whole file; `railway config
plan/apply` to `modula-deploy config plan/apply` (unlisted variables are
deleted unless in `preserve`, as there).
