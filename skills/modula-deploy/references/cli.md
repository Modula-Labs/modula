# `modula-deploy` CLI reference

Binary `modula-deploy` (crate `crates/cli`; install with
`cargo install --path crates/cli`; no prebuilt release yet).
`modula-deploy docs` prints this reference generated from the binary itself;
trust it over this file if they differ.

## Environment

| Variable | Effect |
|---|---|
| `MODULA_URL` | Instance URL; overrides the saved login; default for `login --url`. |
| `MODULA_TOKEN` | API token `mdt_…`; overrides the saved login; default for `login --token`. |
| `MODULA_PROJECT` / `MODULA_ENVIRONMENT` / `MODULA_SERVICE` | Defaults for `-p` / `-e` / `-s`. |
| `NO_COLOR` | No colors (also off when stdout isn't a terminal). |
| `XDG_CONFIG_HOME` | Config dir (default `~/.config`). Login saved to `modula-deploy/config.toml`, mode 0600. |

With `MODULA_URL` and `MODULA_TOKEN` set, no `login` is needed.

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
project ignores the linked environment/service.

## Commands

Setup:
- `login [--url URL] [--token T]`: verifies via `GET /api/auth/me`, saves. Token prompted (no echo) if omitted and stdin is a TTY.
- `logout`: forgets the saved token (it stays valid server-side).
- `whoami`: user, instance, workspaces with roles.
- `list` / `ls`: projects (workspace, name, id, linked).
- `init NAME [-w WORKSPACE]`: create project (+ `production`) and link it.
- `link`: link cwd to `-p`/`-e` (and `-s` as default service).
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
- `environment [NAME]` (`env`): link that environment; no name = list.
- `environment new NAME [--copy ENV] [--copy-variables]`: create (settings from `--copy`, default the first env; volumes empty; no domains/proxies) and link it. Not deployed.
- `domain [--port P]`: generate `<service>[-<env>]-xxxx.<apps domain>`; prints the URL.
- `domain add HOST [--port P]`: custom domain; prints a `TYPE NAME VALUE` table of DNS records to create.
- `domain list` (`ls`) / `domain check HOST` (≥5 s apart) / `domain delete HOST` (`rm`).
- `volume` / `volume add PATH` (attached on next deploy) / `volume delete` (**always fails with a token**: sudo).
- `tcp-proxy` / `tcp-proxy enable [--port P]` (public port 20000–29999, internet-reachable) / `tcp-proxy disable`.

Deploying (no source upload exists: every deploy builds the configured image or the branch's latest commit):
- `up [-d|--detach]`: deploy the selected service; waits, streams build logs (not with `--json`: then only the final deployment), exits 0 on `active`, 1 on failure.
- `deploy [--all] [-w|--wait]`: selected service (`-s`/link) = deploy it; none or `--all` = every service with pending changes in the environment. Prints per-service errors.
- `redeploy [-w]`: the newest deployment again (same image/commit, current variables).
- `restart`: restart the newest `active`/`crashed` deployment's container.
- `down`: remove the newest live deployment.
- `rollback DEPLOYMENT_ID [-w]`: rebuild that deployment's commit (Git-built only).

Databases:
- `db query "SQL" [--write]`: Postgres via the API. Default `readOnly`: exactly one read-only statement, rolled back. `--write`: writes/DDL/multiple statements (Member role, confirmation). Table + `N rows (SELECT, X ms)`; `--json` = `{columns, rows, rowCount, command, durationMs, truncated}`. ≤1000 rows, 30 s timeout, audited. Service: `-s`, else the linked service if it is a database, else the only database.
- `db tables`: schema, name, row estimate.
- `connect [SERVICE] [--tunnel-only] [--show-password] [-P PORT]`: private tunnel (WebSocket `/api/service-instances/{id}/tunnel`, Member role; 30 min idle; ≤16 per user, ≤64 per server), then `psql`/`mysql`/`redis-cli`/`mongosh` with the password in `PGPASSWORD`/`MYSQL_PWD`/`REDISCLI_AUTH`. Credentials only for Postgres today; otherwise, or with `--tunnel-only`, it keeps the tunnel open and prints a connection string (password masked unless `--show-password`).

Shell:
- `completion bash|zsh|fish|elvish|powershell`
- `docs`: full Markdown reference.

## Agent patterns

```bash
modula-deploy status --json | jq '.services[] | {name, status, url}'
printf 'API_KEY=%s\n' "$API_KEY" | modula-deploy variables set --stdin -s api   # secret off the argv
modula-deploy deploy -s api --wait --json        # exit 1 if it fails
modula-deploy logs -s api --build -n 300         # bounded; never -f in automation
modula-deploy db query "select count(*) from users" --json
```

Tunnel for tools that need a real connection (not interactive):

```bash
modula-deploy connect postgres -e staging --tunnel-only -P 54329 --show-password > /tmp/tunnel.log 2>&1 &
TUNNEL=$!
sleep 2
PGURL=$(sed -n 's/^Connection string: //p' /tmp/tunnel.log)   # contains the password; don't print it
pg_dump "$PGURL" --schema-only > schema.sql
kill $TUNNEL; rm -f /tmp/tunnel.log
```

## Not in the CLI

Service settings (start command, health check, builder, resources, replicas),
deleting services/environments/projects, backups, cron, metrics,
`run`/`shell`/`ssh`, scaling, servers: use the HTTP API ([api.md](api.md))
with the same token, or the dashboard.
