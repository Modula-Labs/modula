# `modula` CLI reference

Binary `modula` (crate `crates/cli`; install with
`curl -fsSL https://usemodula.org/install-cli.sh | bash`, or
`cargo install --path crates/cli`). `modula docs` prints the guide and this
reference generated from the binary itself (`modula guide`: the guide only,
incl. `server create` and every non-interactive flag, env var, `--json`
event and exit code); trust them over this file if they differ.

## Environment

| Variable | Effect |
|---|---|
| `MODULA_URL` | Instance URL; overrides the saved login; default for `login --url`. |
| `MODULA_TOKEN` | API token `mdt_…`; overrides the saved login; default for `login --token`. |
| `MODULA_PROJECT` / `MODULA_ENVIRONMENT` / `MODULA_SERVICE` | Defaults for `-p` / `-e` / `-s`. |
| `NO_COLOR` | No colors (also off when stdout isn't a terminal). |
| `XDG_CONFIG_HOME` | Config dir (default `~/.config`). Login saved to `modula/config.toml`, mode 0600. |

With `MODULA_URL` and `MODULA_TOKEN` set, no `login` is needed. Token order:
`MODULA_TOKEN` → nearest `.modula/token` (`login --local`, same TOML format,
mode 0600; its URL, else the global one) → the global config.

## Global flags (any position)

- `--json`: result as JSON on stdout only; hints/progress on stderr. `logs --json` = one JSON object per line.
- `-p, --project <id|name>`: default the linked one.
- `-e, --environment <name|id>`: default the linked one, else `production`.
- `-s, --service <name|id>`: default the linked one, else the project's only service.
- `-y, --yes`: skip confirmations. Without a terminal, destructive commands **require** it (else exit 2).

Confirming commands: `down`, `rollback`, `variables delete`,
`variables set --sealed`, `domain delete`, `volume delete`,
`tcp-proxy disable`, `db query --write`.

Exit codes: 0 ok; 1 error (`error: …` on stderr); 2 usage (bad flags, bad
`KEY=VALUE`, missing `--yes`). `up` and `--wait` variants exit 1 when a
deployment fails/crashes. `connect` returns the client's exit code.

Link file `.modula/project.toml` (nearest in cwd or a parent): `project`,
`project_name`, `environment`, `service`, `service_name`. A `-p` for another
project ignores the linked environment/service. Whenever the CLI writes into
`.modula/` (link, token, secrets) it ensures the repo-root `.gitignore`
ignores `.modula/` (appends an annotated block, creates the file in a git
repo; leaves alone a file naming `.modula/token` and `.modula/secrets.*`).

## Commands

Setup:
- `login [--url URL] [--token T] [--local]`: verifies via `GET /api/auth/me`, saves (globally, or `--local` in `.modula/token`). Token prompted (no echo) if omitted and stdin is a TTY.
- `logout`: forgets the token in effect (`.modula/token` if any, else global; it stays valid server-side).
- `whoami`: user, instance, workspaces with roles.
- `list` / `ls`: projects (workspace, name, id, linked).
- `init NAME [-w WORKSPACE]`: create project (+ `production`) and link it.
- `link [--token T]`: link cwd to `-p`/`-e` (and `-s` as default service); `--token` also saves it in `.modula/token`.
- `unlink`: remove the nearest link.
- `service [NAME]`: set the default service; no name = list services.
- `open`: print the dashboard URL (`/projects/{id}?env=…&service=…`); opens a browser on a TTY.

Status:
- `status`: project, environment, selected service, `pendingChanges`, and per service: newest deployment status (`queued|building|deploying|active|failed|crashed|removed|cancelled`, `null` = never deployed), `url`, `source`, `latestDeploymentId`, `instanceId`.
- `deployment list [-l N]` (`deployments ls`): newest first (default 20, API max 50).
- `logs [-d ID] [-b|--build] [-f|--follow] [-n LINES]`: default newest deployment, last 200 lines (max 5000), no follow. Fails if the deployment's server is offline.

Services:
- `add (-d postgres|mysql|redis|mongo | -i IMAGE | -r OWNER/NAME [-b BRANCH]) [-n NAME] [--port P] [-v KEY=VALUE]…`: creates in every environment, deploys nothing. Name defaults to engine/image/repo name. A new image/repo service becomes the linked default.
- `variables [--shared] [--kv]` (`variable`, `vars`): names, sealed, references, updated. **Values never come back** (masked `********`; `--kv` = `NAME=********`).
- `variables set KEY=VALUE… [--stdin] [--sealed] [--deploy] [--shared]`: split at the first `=`. `--stdin` reads dotenv lines (keeps secrets off the command line). Pending until deployed; `--deploy` deploys now (the service, or with `--shared` every service with pending changes); with `--json` only the deploy result prints. `--skip-deploys` accepted (no-op).
- `variables delete KEY… [--deploy] [--shared]` (`rm`).
- `environment [NAME]` (`env`): link that environment (stored by id); no name = list (marks selected and default).
- `environment new NAME [--duplicate ENV] [--branch SERVICE=BRANCH]…`: create and link it. `--duplicate` copies settings and all variables (sealed stay sealed); without it: empty (default env's settings, no variables). Databases: empty volume, fresh credentials; no domains/proxies. Not deployed.
- `environment rename ENV NAME`.
- `environment delete ENV` (**always fails with a token**: sudo; prints the dashboard link). The default env can't be deleted.
- `domain [--port P]`: generate `<service>[-<env>]-xxxx.<apps domain>`; prints the URL.
- `domain add HOST [--port P]`: custom domain; prints a `TYPE NAME VALUE` table of DNS records to create.
- `domain list` (`ls`) / `domain check HOST` (≥5 s apart) / `domain delete HOST` (`rm`).
- `volume` / `volume add PATH` (attached on next deploy) / `volume delete` (**always fails with a token**: sudo).
- `tcp-proxy` / `tcp-proxy enable [--port P]` (public port 20000–29999, internet-reachable) / `tcp-proxy disable`.

Deploying (`up` uploads local files; the others deploy the configured image or the branch's latest commit):
- `up [--path DIR] [-d|--detach] [-m TEXT] [--ci]`: pack DIR (default cwd; honors `.gitignore`, `.modulaignore`; never `.git/`, `.modula/`; ≤200 MB compressed by default), upload, build like a checkout (service builder/root dir/commands, `railway.toml` in the files) and deploy to the selected service without changing its configured source. Waits, streams build logs (not with `--json`: then only the final deployment), exits 0 on `active`, 1 on failure. `--ci`: no prompts, build logs, runtime logs of a crash. `-m`: message (default: last commit subject). Deployment shows `sourceKind: "upload"`, `source: "Uploaded via CLI"`.
- `deploy [--all] [-w|--wait]`: selected service (`-s`/link) = deploy it; none or `--all` = every service with pending changes in the environment. Prints per-service errors.
- `redeploy [-w]`: the newest deployment again (same image/commit/upload, current variables). Uploads are kept 24 h, or while among the service's 5 newest deployments; else `409`: run `up` again.
- `restart`: restart the newest `active`/`crashed` deployment's container.
- `down`: remove the newest live deployment.
- `rollback DEPLOYMENT_ID [-w]`: rebuild that deployment's commit or kept upload.

Config as code (in the nearest `.modula/`: one of `modula.ts` (run with Node ≥22.18 or Bun; `import … from "modula/iac"`, no install), `modula.toml`, `modula.json`; `--file PATH`):
- `init [NAME] [-p PROJECT] [-e ENV] [--workspace WS] [--format ts|toml] [--yes]`: without a terminal, NAME creates a project, `-p` links an existing one; the config is written only with `--format` (`partial` when this repo's GitHub origin deploys some services but not all). Already linked here → needs `--yes`. `--json` → `{id, name, workspace, environment, created, link, config}`.
- `config init [--force] [--file PATH] [--format ts|toml] [--only SERVICE…]` (`config pull`): write `.modula/modula.ts` (default) or `.toml` from the project, all environments (base from production; TOML: differences in `[environments.<env>]`; TS: `ctx.isEnvironment("env") ? … : …`). `--only` → a `partial` file with those services. No secret values: sealed → `{ secret = true, sealed = true }` / `secret({ sealed: true })`, credential-like names → secrets (`--json` lists them in `guessedSecrets`), template variables omitted.
- `config plan [-e ENV | --all-envs] [--secrets-file PATH] [--from-env] [--update-sealed]`: diff vs the server. `--json` → `{project, changes, environments:[{environment, createEnvironment, services:[{service, action: create|update|unchanged, settings:[{key,from,to}], variables:[{name, op: add|update|delete, secret, sealed, from?, to?}], domains:[{op,host,…}], volume?, tcpProxy}], shared, manual, warnings, errors}]}`. Exit 1 if `errors`.
- `config apply [same] [--yes] [--deploy]`: confirm, then create environments/services, PATCH settings, volumes, TCP proxies, variables, domains; `--deploy` deploys changed services. Manual (sudo): deleting services/volumes, moving volumes. Refuses if any error (e.g. secret without a value).
- `config validate [--file PATH]`: offline check of the file and every overlay.
- File rules: settings keys = API `config` keys (camelCase; unknown rejected); omitted settings = defaults (template's for templates); `source = { repo, branch, rootDir? } | { image } | { local = "." }`; `domains` = custom only (generated never deleted; `generateDomain = true`); `volume = "/path"`; `tcpProxy = PORT`; variables: string/number/bool/`{ secret = true[, sealed = true] }`; unlisted variables deleted unless in `preserve` or created by the template. Secrets from `.modula/secrets.<env>.env` (dotenv, flat names), `--secrets-file`, or `--from-env`.

Databases:
- `db query "SQL" [--write]`: Postgres via the API. Default `readOnly`: exactly one read-only statement, rolled back. `--write`: writes/DDL/multiple statements (Member role, confirmation). Table + `N rows (SELECT, X ms)`; `--json` = `{columns, rows, rowCount, command, durationMs, truncated}`. ≤1000 rows, 30 s timeout, audited. Service: `-s`, else the linked service if it is a database, else the only database.
- `db tables`: schema, name, row estimate.
- `connect [SERVICE] [--tunnel-only] [--show-password] [-P PORT]`: private tunnel (WebSocket `/api/service-instances/{id}/tunnel`, Member role; 30 min idle; ≤16 per user, ≤64 per server), then `psql`/`mysql`/`redis-cli`/`mongosh` with the password in `PGPASSWORD`/`MYSQL_PWD`/`REDISCLI_AUTH`. Credentials only for Postgres today; otherwise, or with `--tunnel-only`, it keeps the tunnel open and prints a connection string (password masked unless `--show-password`).

Shell:
- `completion bash|zsh|fish|elvish|powershell`
- `docs`: full Markdown reference.

## Agent patterns

```bash
modula status --json | jq '.services[] | {name, status, url}'
printf 'API_KEY=%s\n' "$API_KEY" | modula variables set --stdin -s api   # secret off the argv
modula deploy -s api --wait --json        # exit 1 if it fails
modula logs -s api --build -n 300         # bounded; never -f in automation
modula db query "select count(*) from users" --json
```

Tunnel for tools that need a real connection (not interactive):

```bash
modula connect postgres -e staging --tunnel-only -P 54329 --show-password > /tmp/tunnel.log 2>&1 &
TUNNEL=$!
sleep 2
PGURL=$(sed -n 's/^Connection string: //p' /tmp/tunnel.log)   # contains the password; don't print it
pg_dump "$PGURL" --schema-only > schema.sql
kill $TUNNEL; rm -f /tmp/tunnel.log
```

## Not in the CLI

Editing a single service setting directly (use `config apply` or the HTTP
API), deleting services/environments/projects, backups, cron, metrics,
`run`/`shell`/`ssh`, scaling, servers: use the HTTP API ([api.md](api.md))
with the same token, or the dashboard.
