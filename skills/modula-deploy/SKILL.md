---
name: modula-deploy
description: Use when deploying, inspecting or managing apps on a Modula Deploy instance (a self-hosted, Railway-style PaaS) through the `modula-deploy` CLI or its HTTP API - creating services, setting variables and references, deploying, checking status and logs, rolling back, adding domains, or connecting to and querying databases. Triggers on MODULA_TOKEN/MODULA_URL, `.modula/project.toml`, `modula-deploy` commands, `${{service.VAR}}` references, or a user asking to ship/debug something on Modula Deploy.
---

# Modula Deploy

Modula Deploy runs projects → environments → services → deployments on the
user's own servers. You operate it with an API token through the
`modula-deploy` CLI (projects, services, variables, deploys, logs, domains,
databases; always pass `--json` when you parse output) and, for what the CLI
lacks (service settings), the HTTP API. References:
[cli](references/cli.md), [api](references/api.md),
[variables](references/variables.md).

## Auth

- Expect `MODULA_URL` (e.g. `https://deploy.example.com`) and `MODULA_TOKEN`
  (`mdt_…`) in the environment, or a saved login
  (`~/.config/modula-deploy/config.toml`). Check with `modula-deploy whoami`.
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
  (`modula-deploy volume delete` always fails with a token)
- revealing a variable value (values are write-only over the API; the CLI
  shows `********`)
- restoring or deleting a backup, deleting a backup target
- rotating a database password, creating join tokens or tokens
- connecting Cloudflare/Hetzner, creating or deleting Hetzner servers (these
  also cost money: only ever suggest them, via `GET …/servers/suggest`)

Roles matter too (Viewer < Deployer < Member < Admin): variables and deploys
need Deployer; settings, domains, tunnels and the Database API need Member.
`403 forbidden` means the role is too low.

## Orient first

```bash
modula-deploy whoami --json                 # workspaces + role
modula-deploy list --json                   # projects
modula-deploy link -p shop -e production    # or pass -p/-e/-s on every command
modula-deploy status --json                 # services: status, url, source, pending changes
```

Almost every action targets a **service instance** (one service in one
environment). The CLI resolves it from `-p/-e/-s` or `.modula/project.toml`
(`modula-deploy service <name>` sets the default service). For raw API calls,
`status --json` gives each service's `instanceId`.

## Workflows

**Create a service** (Member): `modula-deploy add --database postgres`
(`mysql`, `redis`, `mongo`), `add --image ghcr.io/acme/api:1.2 --port 3000`, or
`add --repo owner/name --branch main --port 3000` (needs the workspace's
GitHub App). Names: lowercase letters, digits, `-`. The service exists in
every environment; nothing is deployed yet.

**Change settings** (Member, API only): `GET /api/service-instances/{id}` →
edit `.config` → `PATCH` with `{"config": …}` (send the whole object; unknown
fields are rejected). Field reference: [api → settings](references/api.md#service-settings-config).

**Set variables** (Deployer):
`modula-deploy variables set -s api 'DATABASE_URL=${{postgres.DATABASE_URL}}'`;
secrets through stdin so they stay off the command line:
`printf 'API_KEY=%s\n' "$API_KEY" | modula-deploy variables set --stdin -s api`.
`--shared` for environment-wide ones. Prefer references over copied values.
Use `--sealed` only when the user agrees (permanent, unrevealable). Listing
shows names only. See [variables](references/variables.md).

**Deploy** (Deployer): settings and variable edits stay pending
(`status --json` → `pendingChanges`). `modula-deploy deploy -s api --wait`
deploys one service and exits 1 if it fails; `deploy --all --wait` deploys
every service with pending changes. `up` = deploy the selected service and
follow its build. **Nothing uploads local files**: deploys build the
configured image or the branch's latest commit, so push first.

**Status and logs**: `status --json`, `deployment list --json` (newest
first, with `error` on failures), and bounded logs:
`modula-deploy logs -s api --build -n 300` (runtime logs without `--build`).
Never use `logs -f` in automation; use `deploy --wait` to wait.

**Rollback / redeploy / restart / down** (Deployer):
`modula-deploy rollback <deploymentId> --yes --wait` (Git-built only; rebuilds
that commit), `redeploy --wait` (same source, current variables), `restart`
(active/crashed only), `down --yes` (removes the live deployment). Pick the
target from `deployment list` and confirm with the user first.

**Domains** (Member): `modula-deploy domain` generates one;
`domain add api.example.com` prints the DNS records to give the user
(CNAME/A + `TXT _modula-challenge.<host>`); `domain check api.example.com`
(≥5 s apart) until `verified`. Behind Cloudflare: DNS-only until verified and
serving HTTPS, then proxied with SSL mode Full (strict).

**Database access** (Member):
- Queries: `modula-deploy db query "select …" --json` runs **one read-only
  statement** in a rolled-back READ ONLY transaction (API:
  `POST …/database/query {"sql","readOnly":true}`). Writes need
  `--write --yes` (the API call without `readOnly`), after the user agrees.
  ≤1000 rows, 30 s timeout, audited. `db tables` lists tables. Postgres only.
- Interactive: `modula-deploy connect [service]` starts `psql` for Postgres;
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
| `not logged in` | Set `MODULA_URL` and `MODULA_TOKEN`, or ask the user to `modula-deploy login`. |
| Exit code 2, "Pass --yes to confirm" | A destructive command without a terminal: confirm with the user, then add `--yes`. |
| "pass --service" | The project has several services: add `-s <name>` or run `modula-deploy service <name>`. |
| `401 unauthorized` | Token wrong, expired or deleted. Ask for a new one. |
| `404 not_found` | Wrong id, or the token is scoped to another workspace. |
| `403 sudo_required` / "needs a recently re-authenticated dashboard session" | Dashboard-only action: hand it to the user. |
| `409` "has no server" / "server … is offline" | Logs/DB API need the server online; check `GET /api/nodes` (admins) or ask the user. |
| Build `failed` | Read `kind=build` logs: builder, `rootDir`, `dockerfilePath`, missing build vars. |
| "Health check /x did not pass within Ns" | App must listen on `0.0.0.0:$PORT`; check the path; raise `healthcheckTimeoutSecs`. |
| "Pre-deploy command exited with code N" | Migration failed; the error has the output tail; old deployment still serves. |
| Variable empty at runtime | Reference to a missing variable/service resolves to `""`; service names in references are lowercase. |
| Change has no effect | It's pending: `modula-deploy deploy` (or `--all`). |
| Generated domain fails "set MODULA_APPS_DOMAIN" | Instance has no apps domain; use a custom domain or ask the admin. |
| Custom domain stuck `pending` | TXT record missing at `_modula-challenge.<host>`; DNS can take minutes. |
| `connect` keeps only a tunnel | Not Postgres, or no client installed: connect a client to the printed port. |
