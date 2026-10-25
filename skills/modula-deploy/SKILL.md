---
name: modula-deploy
description: Use when deploying, inspecting or managing apps on a Modula Deploy instance (a self-hosted, Railway-style PaaS) through the `modula` CLI or its HTTP API - creating services, setting variables and references, deploying, checking status and logs, rolling back, adding domains, or connecting to and querying databases. Triggers on MODULA_TOKEN/MODULA_URL, `.modula/project.toml`, `modula` commands, `${{service.VAR}}` references, or a user asking to ship/debug something on Modula Deploy.
---

# Modula Deploy

Modula Deploy runs projects → environments → services → deployments on the
user's own servers. You operate it with an API token through the
`modula` CLI (projects, services, variables, deploys, logs, domains,
databases; always pass `--json` when you parse output), `.modula/modula.ts` config
as code (`config plan`/`config apply`) for settings across services and
environments, and the HTTP API for the rest. References:
[cli](references/cli.md), [api](references/api.md),
[variables](references/variables.md). `modula guide` prints the guide for
people and agents (install, create a server, log in, deploy, every
non-interactive flag, env var, `--json` event and exit code,
troubleshooting); `modula docs` adds the full command reference.

## Auth

- Expect `MODULA_URL` (e.g. `https://deploy.example.com`) and `MODULA_TOKEN`
  (`mdt_…`) in the environment, or a saved login: this directory's
  `.modula/token` (from `login --local`) or `~/.config/modula/config.toml`,
  in that order after `MODULA_TOKEN`. Check with `modula whoami`.
  Never commit `.modula/` (the CLI adds it to `.gitignore` itself).
- If there's no token, ask the user to create one (it needs their dashboard
  session with re-authentication; you cannot mint one) and to export it
  themselves. Suggest a workspace-scoped token with an expiry.
- **Never print, echo, log or paste the token.** Use `"$MODULA_TOKEN"` in
  commands; don't pass it as `--token` on a command line you show.
- API calls: `curl -fsS -H "Authorization: Bearer $MODULA_TOKEN" -H 'content-type: application/json' "$MODULA_URL/api/…"`.
  Define a helper once:

  ```bash
  api() { curl -fsS -H "Authorization: Bearer $MODULA_TOKEN" -H 'content-type: application/json' "$@"; }
  ```

## What a token cannot do

Tokens never have sudo. These return `403 sudo_required` and must be done by
the user in the dashboard; don't retry or look for workarounds:

- deleting a workspace, project, environment, service or volume
  (`modula volume delete` always fails with a token)
- revealing a variable value (values are write-only over the API; the CLI
  shows `********`). Exception: `config plan`/`config pull` read non-sealed
  values through an audited Member route; never print them.
- restoring or deleting a backup, deleting a backup target
- rotating a database password, creating join tokens or tokens
- connecting Cloudflare/Hetzner, creating or deleting Hetzner worker servers
  from the instance (these also cost money: only ever suggest them, via
  `GET …/servers/suggest`). Creating a *new main server* with the user's own
  Hetzner token is a different thing: see above.

Roles matter too (Viewer < Deployer < Member < Admin): variables and deploys
need Deployer; settings, domains, tunnels and the Database API need Member.
`403 forbidden` means the role is too low.

## No instance yet: create one (only when the user asks)

`modula server create` builds a whole Modula server on the user's Hetzner
Cloud account from here: firewall, server, DNS (optionally via Cloudflare),
install, admin account, and a saved login. **It costs money every month:
only run it when the user explicitly asks, and confirm location, type and
the monthly price with them first.** Secrets come from the environment the
user sets (never ask them to paste tokens into chat, never put secrets in
flags; the CLI refuses those):

```bash
# The user exports: HCLOUD_TOKEN (Read & Write), MODULA_ADMIN_PASSWORD (≥10 chars),
# and CLOUDFLARE_API_TOKEN if the domain is on Cloudflare.
modula server options --json                  # locations, types in stock, prices, .recommended
modula server create --provider hetzner --location fsn1 --type cx33 \
  --name modula-main --domain deploy.example.com --email you@example.com \
  --ssh-key-file ~/.ssh/id_ed25519.pub --admin-name 'Ada' \
  --cloudflare-dns --yes --json               # or without --cloudflare-dns: see below
```

- It never prompts without a terminal or with `--json`: anything missing
  exits **2** listing every missing flag/variable, before anything is created.
- `--json` streams one event per line (`step`, `server_created`,
  `dns_waiting`, `install_failed`, …) and ends with a `result` line.
- Without `--cloudflare-dns`, the user must add the A record: run it in the
  background, read the `dns_waiting` event (`name`, `value`) and tell the
  user right away; it waits up to `--dns-timeout` minutes (30).
- Exit **0**: ready and logged in (`modula whoami` works). **1**: failed and
  nothing is left running (by default it deletes the server on failure).
  **3**: a server exists but isn't fully set up; relay the result's `hint`.
- Takes ~5–10 minutes; don't kill it midway (that leaves a billing server
  unless it gets to clean up).

## Orient first

```bash
modula whoami --json                 # workspaces + role
modula list --json                   # projects
modula link -p shop -e production    # or pass -p/-e/-s on every command
modula status --json                 # services: status, url, source, pending changes
```

Almost every action targets a **service instance** (one service in one
environment). The CLI resolves it from `-p/-e/-s` or `.modula/project.toml`
(`modula service <name>` sets the default service). For raw API calls,
`status --json` gives each service's `instanceId`.

## Workflows

**Create a service** (Member): `modula add --database postgres`
(`mysql`, `redis`, `mongo`), `add --image ghcr.io/acme/api:1.2 --port 3000`, or
`add --repo owner/name --branch main --port 3000` (needs the workspace's
GitHub App). A whole app with its databases, deployed at once:
`add --template n8n` (`--template list` for the gallery). Names: lowercase letters, digits, `-`. The service exists in
every environment; nothing is deployed yet.

**Config as code** (Member): if the repo has `.modula/modula.ts` or
`.modula/modula.toml` (or the user wants one), change settings, variables, domains and volumes there, not by hand:
`modula config validate`, `config plan --json` (show the user the
plan; secrets are masked), then `config apply --yes [--deploy]` after they
agree. `config pull` writes the file from the server (no secret values).
Secrets are `NAME = { secret = true }` with values in
`.modula/secrets.<env>.env` or `--from-env`; settings left out reset to
defaults; unlisted variables are deleted unless in `preserve`. Deleting
services/volumes stays manual (plan lists it). Guide: `guide/config-as-code.md`.

**Change settings** (Member) without a file: `GET /api/service-instances/{id}` →
edit `.config` → `PATCH` with `{"config": …}` (send the whole object; unknown
fields are rejected). Field reference: [api → settings](references/api.md#service-settings-config).

**Set variables** (Deployer):
`modula variables set -s api 'DATABASE_URL=${{postgres.DATABASE_URL}}'`;
secrets through stdin so they stay off the command line:
`printf 'API_KEY=%s\n' "$API_KEY" | modula variables set --stdin -s api`.
`--shared` for environment-wide ones. Prefer references over copied values.
Use `--sealed` only when the user agrees (permanent, unrevealable). Listing
shows names only. See [variables](references/variables.md).

**Deploy** (Deployer): settings and variable edits stay pending
(`status --json` → `pendingChanges`). `modula deploy -s api --wait`
deploys one service and exits 1 if it fails; `deploy --all --wait` deploys
every service with pending changes; they build the configured image or the
branch's latest commit, so push first. `up --ci` = upload the current
directory (`--path DIR`; honors `.gitignore`/`.modulaignore`) to the selected
service, build and deploy it without GitHub, stream the build, exit 1 unless
it goes live. It doesn't change the service's configured source (the next
`deploy` deploys that again).

**Status and logs**: `status --json`, `deployment list --json` (newest
first, with `error` on failures), and bounded logs:
`modula logs -s api --build -n 300` (runtime logs without `--build`).
Never use `logs -f` in automation; use `deploy --wait` to wait.

**Rollback / redeploy / restart / down** (Deployer):
`modula rollback <deploymentId> --yes --wait` (Git-built, or uploads
still kept 24 h / among the 5 newest; rebuilds that source), `redeploy --wait` (same source, current variables), `restart`
(active/crashed only), `down --yes` (removes the live deployment). Pick the
target from `deployment list` and confirm with the user first.

**Domains** (Member): `modula domain` generates one;
`domain add api.example.com` prints the DNS records to give the user
(CNAME/A + `TXT _modula-challenge.<host>`); `domain check api.example.com`
(≥5 s apart) until `verified`. Behind Cloudflare: DNS-only until verified and
serving HTTPS, then proxied with SSL mode Full (strict).

**Database access** (Member):
- Queries: `modula db query "select …" --json` runs **one read-only
  statement** in a rolled-back READ ONLY transaction (API:
  `POST …/database/query {"sql","readOnly":true}`). Writes need
  `--write --yes` (the API call without `readOnly`), after the user agrees.
  ≤1000 rows, 30 s timeout, audited. `db tables` lists tables. Postgres only.
- Interactive: `modula connect [service]` starts `psql` for Postgres;
  for MySQL/Redis/Mongo it only opens the tunnel. Agents can't drive an
  interactive client: use `--tunnel-only -P <port>` in the background, run
  one-shot commands, then stop the tunnel (pattern in [cli](references/cli.md)).
- Credentials: `GET …/database/credentials` returns the password. Use it in
  commands, never show it.

## Safety rules

1. **Confirm before anything destructive or user-visible**: deploying to
   production, rollback, removing a deployment, deleting domains, TCP proxies
   or variables, row edits, and any SQL that writes (`INSERT/UPDATE/DELETE/
   ALTER/DROP/TRUNCATE`). State exactly what will change and in which
   environment. Prefer doing it in staging first.
2. **Never expose a database publicly without asking.** Don't run
   `tcp-proxy enable` on your own; use `db query` or `connect` tunnels. If a
   proxy was created for a task, offer to remove it (`tcp-proxy disable`).
3. Prefer `--tunnel-only` for GUI tools and scripts.
4. **Don't paste secrets into chat**: tokens, variable values, connection
   strings with passwords, `credentials` responses, or log lines that look
   unmasked. Refer to them by name.
5. Read before write: fetch the current config/variables, change the minimum,
   and send back the full object.
6. Don't loop on `409 conflict` or `403`: read the message and report it.

## Troubleshooting

| Symptom | Do |
|---|---|
| `not logged in` | Set `MODULA_URL` and `MODULA_TOKEN`, or ask the user to `modula login`. |
| Exit code 2, "Pass --yes to confirm" | A destructive command without a terminal: confirm with the user, then add `--yes`. |
| "pass --service" | The project has several services: add `-s <name>` or run `modula service <name>`. |
| `401 unauthorized` | Token wrong, expired or deleted. Ask for a new one. |
| `404 not_found` | Wrong id, or the token is scoped to another workspace. |
| `403 sudo_required` / "needs a recently re-authenticated dashboard session" | Dashboard-only action: hand it to the user. |
| `409` "has no server" / "server … is offline" | Logs/DB API need the server online; check `GET /api/nodes` (admins) or ask the user. |
| Build `failed` | Read `kind=build` logs: builder, `rootDir`, `dockerfilePath`, missing build vars. |
| "Health check /x did not pass within Ns" | App must listen on `0.0.0.0:$PORT`; check the path; raise `healthcheckTimeoutSecs`. |
| "Pre-deploy command exited with code N" | Migration failed; the error has the output tail; old deployment still serves. |
| Variable empty at runtime | Reference to a missing variable/service resolves to `""`; service names in references are lowercase. |
| Change has no effect | It's pending: `modula deploy` (or `--all`). |
| `config apply`: "is a secret that doesn't exist yet" | Ask the user to put the value in `.modula/secrets.<env>.env` (or export it and pass `--from-env`); never ask for it in chat. |
| `config plan` keeps showing a change | A sealed variable written as a plain value (write `{ secret = true, sealed = true }`), or a setting the server normalizes: check `--json` `from`/`to`. |
| Generated domain fails "set MODULA_APPS_DOMAIN" | Instance has no apps domain; use a custom domain or ask the admin. |
| Custom domain stuck `pending` | TXT record missing at `_modula-challenge.<host>`; DNS can take minutes. |
| `connect` keeps only a tunnel | Not Postgres, or no client installed: connect a client to the printed port. |
