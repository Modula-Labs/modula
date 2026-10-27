# HTTP API

Everything the dashboard does goes through a JSON API under `/api` on your
instance. The `modula` CLI uses the same API, and so can your scripts.

## Authenticate with an API token

1. Create a token (`POST /api/account/tokens`; see
   [security → API tokens](security.md#api-tokens) for how to do it from the
   dashboard). You can restrict it to one workspace and give it an expiry
   (1–3650 days, or none). Creating a token needs a recent re-authentication
   (sudo), and the token (`mdt_…`) is shown once.
2. Send it as a bearer token:

```bash
export MODULA_URL=https://deploy.example.com
export MODULA_TOKEN=mdt_...   # keep it out of shell history and logs

curl -fsS -H "Authorization: Bearer $MODULA_TOKEN" "$MODULA_URL/api/auth/me"
```

What a token can and can't do:

- It acts as you, with your role in each workspace.
- A token restricted to a workspace reaches nothing outside it, and never
  reaches instance-admin routes (servers, join tokens, the GitHub App, the
  control-plane backup).
- **A token never has sudo.** Every route marked *sudo* below (deleting a
  project, environment, service or volume, revealing a variable, restoring or
  deleting a backup, creating join tokens, rotating a database password,
  connecting Cloudflare or Hetzner, creating servers, creating another
  token…) only works from a dashboard session.
- With an `Authorization` header the server never falls back to cookies, and
  token requests are exempt from the browser `Origin` check.
- Logging out of the CLI does not revoke a token. Delete it with
  `DELETE /api/account/tokens/{token_id}`.

## Conventions

- Bodies are JSON with `camelCase` fields. Unknown fields are rejected.
- Ids are 16-character lowercase base36 strings.
- Non-members get `404` for resources in other workspaces (not `403`).
- Errors have one shape:

```json
{ "error": { "code": "conflict", "message": "a service named api already exists" } }
```

| HTTP | `code` | Meaning |
|---|---|---|
| 400 | `bad_request` | Validation failed; `message` says what. |
| 401 | `unauthorized` / `invalid_credentials` | Missing, expired or wrong token/session. |
| 403 | `forbidden` | Your role is too low, or a workspace token hit an instance route. |
| 403 | `sudo_required` | The route needs sudo: do it in the dashboard. |
| 404 | `not_found` | Doesn't exist, or you can't see it. |
| 409 | `conflict` | State doesn't allow it (name taken, nothing running, server offline…). |
| 429 | `rate_limited` | Slow down (e.g. domain "Check now" cooldown). |
| 502 | `upstream_unavailable` | A dependency (GitHub, a server) didn't answer. |

## Roles

Workspace roles, lowest to highest: **Viewer < Deployer < Member < Admin**.
A route needs at least the role in the table. "Own" means the resource must be
your own (sessions, tokens). "Instance admin" means an instance administrator,
regardless of workspace roles.

## Route reference

Access column: **Public** (no auth), **Auth** (any signed-in user or token),
**Sudo** (signed-in with recent re-authentication; never a token), a role, or
**Instance admin**. "+ sudo" means a dashboard session with recent
re-authentication is also required.

### Instance and install

| Method | Path | Access |
|---|---|---|
| GET | `/api/health` | Public |
| GET | `/api/maintenance` | Public (main server migration: `{mode, message, newUrl}`; writes get 503 `maintenance`, a retired dashboard 503 `moved` with `movedTo`) |
| GET | `/api/instance` | Public (`setupRequired`, `version`) |
| GET | `/api/install.sh` | Public (installer for joining servers) |
| GET | `/api/releases` | Public (release files for joining servers) |

### Auth and account

| Method | Path | Access |
|---|---|---|
| POST | `/api/auth/setup` | Public (one-time setup token) |
| POST | `/api/auth/login` | Public |
| GET | `/api/auth/mfa` | Pending 2FA session |
| POST | `/api/auth/mfa` | Pending 2FA session |
| POST | `/api/auth/mfa/passkey` | Pending 2FA session |
| POST | `/api/auth/sudo` | Auth |
| POST | `/api/auth/sudo/passkey` | Auth |
| POST | `/api/auth/logout` | Auth |
| GET | `/api/auth/me` | Auth |
| GET | `/api/account/security` | Auth |
| POST | `/api/account/totp` | Sudo |
| POST | `/api/account/totp/confirm` | Sudo |
| DELETE | `/api/account/totp` | Sudo |
| POST | `/api/account/passkeys` | Sudo |
| POST | `/api/account/passkeys/confirm` | Sudo |
| PATCH | `/api/account/passkeys/{passkey_id}` | Own |
| DELETE | `/api/account/passkeys/{passkey_id}` | Own + sudo |
| POST | `/api/account/backup-codes` | Sudo |
| POST | `/api/account/password` | Sudo |
| GET | `/api/account/sessions` | Auth |
| DELETE | `/api/account/sessions/{session_id}` | Own |
| GET | `/api/account/tokens` | Auth |
| POST | `/api/account/tokens` | Sudo |
| DELETE | `/api/account/tokens/{token_id}` | Own |

`POST /api/account/tokens` body: `{ "name": "ci", "workspaceId": null, "expiresInDays": 90 }`.

### Workspaces, projects, environments

| Method | Path | Access |
|---|---|---|
| GET | `/api/workspaces` | Auth |
| POST | `/api/workspaces` | Auth |
| GET | `/api/workspaces/{workspace_id}` | Viewer |
| PATCH | `/api/workspaces/{workspace_id}` | Admin |
| DELETE | `/api/workspaces/{workspace_id}` | Admin + sudo |
| GET | `/api/workspaces/{workspace_id}/projects` | Viewer |
| POST | `/api/workspaces/{workspace_id}/projects` | Member |
| GET | `/api/projects/{project_id}` | Viewer |
| PATCH | `/api/projects/{project_id}` | Member |
| DELETE | `/api/projects/{project_id}` | Admin + sudo |
| POST | `/api/projects/{project_id}/environments` | Member |
| PATCH | `/api/environments/{environment_id}` | Member |
| DELETE | `/api/environments/{environment_id}` | Admin + sudo |
| POST | `/api/environments/{environment_id}/sync` | Member |
| GET | `/api/projects/{project_id}/activity` | Viewer |
| GET | `/api/environments/{environment_id}/config` | Member (audited) |
| GET | `/api/environments/{environment_id}/pending` | Viewer |
| GET | `/api/environments/{environment_id}/warnings` | Viewer |
| GET | `/api/environments/{environment_id}/placement-suggestion` | Viewer |
| POST | `/api/environments/{environment_id}/deploy` | Deployer |

- `GET /api/environments/{id}/config` returns the whole environment for
  [config as code](config-as-code.md): each service's `config`, `template`
  (with `templateConfig` and `templateVariables`), `volume`, `tcpProxy`,
  `domains` and `variables` (`name`, `sealed`, and `value` for non-sealed
  ones), plus `shared` variables. It is the one route that returns variable
  values without sudo (never sealed ones); every call is audited as
  `environment.config_read` with the names returned.
- `GET /api/projects/{id}` returns `project`, `environments`, `services` and
  `instances` (one per service per environment, with `latestDeploymentStatus`,
  `domain`, `dependsOn`…). Use it to find a service instance id.
- Create environment: `{ "name": "staging", "duplicateFrom": "<env id>", "branches": { "<service id>": "staging" } }`
  (both optional: without `duplicateFrom` the environment is empty — the
  default environment's service settings, no variables).
- Rename environment: `{ "name": "preview" }`. The default (oldest)
  environment, `isDefault` in the project detail, can't be deleted.
- Sync: `{ "fromEnvironmentId": "<env id>" }`.
- Activity query: `?environmentId=&serviceId=&kind=deployment|variable|config|service`.
- Deploy pending changes: `{ "serviceIds": ["…"] }` (omit for all services with changes).
- Warnings: `{ "items": [{ "kind", "severity", "serviceId", "serviceInstanceId",
  "title", "detail", "docsUrl", "action" }] }`, worst first. `kind` is
  `deploy_failed`, `crashed`, `server_offline`, `main_server`, `disk_full` or
  `missing_reference`; `severity` is `error`, `warning` or `info`; `action` is
  `{ "type": "openDeployment", "deploymentId" }`, `{ "type": "moveServer",
  "nodeId" }` (the recommended server), `{ "type": "addServer" }`,
  `{ "type": "openServer", "nodeId" }` or `{ "type": "openVariables" }`.
- Placement suggestion: `?instance=<instance id>` and/or
  `?references=postgres,redis` (service names a new service will reference).
  Returns `{ "servers": [{ "nodeId", "name", "online", "main", "locationId",
  "recommended" }] }`, best first: the order the first deployment uses (see
  [placement](servers.md#placement)).

### Services, settings, variables

| Method | Path | Access |
|---|---|---|
| POST | `/api/projects/{project_id}/services` | Member |
| PATCH | `/api/services/{service_id}` | Member |
| GET | `/api/services/{service_id}/rename-preview` | Member |
| DELETE | `/api/services/{service_id}?allEnvironments=true` (every environment; refused without the flag) | Member + sudo |
| GET | `/api/service-instances/{instance_id}` | Viewer |
| PATCH | `/api/service-instances/{instance_id}` | Member |
| DELETE | `/api/service-instances/{instance_id}` (the service in one environment; the last one deletes it) | Member + sudo |
| GET | `/api/service-instances/{instance_id}/variables` | Viewer |
| PATCH | `/api/service-instances/{instance_id}/variables` | Deployer |
| GET | `/api/environments/{environment_id}/variables` | Viewer |
| PATCH | `/api/environments/{environment_id}/variables` | Deployer |
| GET | `/api/environments/{environment_id}/variable-names` | Viewer |
| POST | `/api/variables/{variable_id}/reveal` | Member + sudo |

- Create a service: `{ "name": "api", "config": { … } }` or `{ "name": "postgres", "template": "postgres" }`.
  See [settings](projects-and-services.md#settings-reference) for `config`.
- Update settings: `PATCH` with `{ "config": { … } }` (the whole config).
- Variables: `PATCH` with `{ "set": { "NAME": "value" }, "sealed": ["NAME"], "delete": ["OLD"] }`.
  `GET` returns names and metadata only, never values.

### Deployments

| Method | Path | Access |
|---|---|---|
| POST | `/api/service-instances/{instance_id}/deployments` | Deployer |
| GET | `/api/service-instances/{instance_id}/deployments` | Viewer |
| GET | `/api/deployments/{deployment_id}` | Viewer |
| DELETE | `/api/deployments/{deployment_id}` | Deployer |
| GET | `/api/deployments/{deployment_id}/logs` | Viewer |
| POST | `/api/deployments/{deployment_id}/redeploy` | Deployer |
| POST | `/api/deployments/{deployment_id}/rollback` | Deployer |
| POST | `/api/deployments/{deployment_id}/restart` | Deployer |

Logs are Server-Sent Events (`log`, `error`, `end`). Query: `tail` (default
200, max 5000), `follow` (default `true`), `kind` = `runtime` (default) or `build`.
Each `log` event is `{ "ts": <unix ms>, "stderr": bool, "text": "…" }`, with
variable values masked.

```bash
curl -N -H "Authorization: Bearer $MODULA_TOKEN" \
  "$MODULA_URL/api/deployments/$DEP/logs?kind=build&follow=false&tail=500"
```

### Domains

| Method | Path | Access |
|---|---|---|
| GET | `/api/service-instances/{instance_id}/domains` | Viewer |
| POST | `/api/service-instances/{instance_id}/domains` | Member (generate a domain) |
| POST | `/api/service-instances/{instance_id}/custom-domains` | Member |
| POST | `/api/domains/{domain_id}/check` | Member |
| PATCH | `/api/domains/{domain_id}` | Member |
| DELETE | `/api/domains/{domain_id}` | Member |

Custom domain: `{ "hostname": "api.example.com", "targetPort": null }`.
Update (both fields replace the stored values; `null` = default):
`{ "targetPort": 8080, "rateLimitPerMinute": 600 }`. See [domains](domains.md#api).

### Databases, volumes, TCP proxies, tunnels

| Method | Path | Access |
|---|---|---|
| GET | `/api/templates` | Auth |
| POST | `/api/service-instances/{instance_id}/volume` | Member |
| DELETE | `/api/volumes/{volume_id}` | Admin + sudo |
| POST | `/api/service-instances/{instance_id}/tcp-proxy` | Member |
| DELETE | `/api/tcp-proxies/{tcp_proxy_id}` | Member |
| GET | `/api/service-instances/{instance_id}/tunnel` | Member (WebSocket; `?port=`) |

Volume: `{ "mountPath": "/data" }`. TCP proxy: `{ "targetPort": 5432 }`.

### Database tab (Postgres services)

| Method | Path | Access |
|---|---|---|
| GET | `/api/service-instances/{instance_id}/database/tables` | Member |
| GET | `/api/service-instances/{instance_id}/database/rows` | Member |
| POST | `/api/service-instances/{instance_id}/database/rows` | Member |
| PATCH | `/api/service-instances/{instance_id}/database/rows` | Member |
| DELETE | `/api/service-instances/{instance_id}/database/rows` | Member |
| POST | `/api/service-instances/{instance_id}/database/query` | Member |
| GET | `/api/service-instances/{instance_id}/database/stats` | Member |
| POST | `/api/service-instances/{instance_id}/database/vacuum` | Member |
| GET | `/api/service-instances/{instance_id}/database/extensions` | Member |
| POST | `/api/service-instances/{instance_id}/database/extensions` | Member |
| GET | `/api/service-instances/{instance_id}/database/credentials` | Member |
| POST | `/api/service-instances/{instance_id}/database/password` | Member + sudo |

See [databases](databases.md#database-tab-and-api) for bodies and limits.

### Backups

| Method | Path | Access |
|---|---|---|
| GET | `/api/workspaces/{workspace_id}/backup-targets` | Member |
| POST | `/api/workspaces/{workspace_id}/backup-targets` | Admin |
| DELETE | `/api/backup-targets/{backup_target_id}` | Admin + sudo |
| GET | `/api/service-instances/{instance_id}/backups` | Viewer |
| POST | `/api/service-instances/{instance_id}/backups` | Deployer |
| PUT | `/api/service-instances/{instance_id}/backup-schedule` | Member |
| POST | `/api/backups/{backup_id}/restore` | Member + sudo |
| DELETE | `/api/backups/{backup_id}` | Member + sudo |
| GET | `/api/instance/self-backup` | Instance admin |
| PUT | `/api/instance/self-backup` | Instance admin + sudo |

### Servers

| Method | Path | Access |
|---|---|---|
| GET | `/api/nodes` | Instance admin |
| POST | `/api/nodes/join-tokens` | Instance admin + sudo |
| PATCH | `/api/nodes/{node_id}` | Instance admin (`{ "location": "<id>" \| null }`) |
| DELETE | `/api/nodes/{node_id}` | Instance admin + sudo |
| GET | `/api/locations` | Auth |

### Cloudflare integration

| Method | Path | Access |
|---|---|---|
| GET | `/api/workspaces/{workspace_id}/integrations/cloudflare` | Member |
| POST | `/api/workspaces/{workspace_id}/integrations/cloudflare/verify` | Admin + sudo |
| PUT | `/api/workspaces/{workspace_id}/integrations/cloudflare` | Admin + sudo |
| DELETE | `/api/workspaces/{workspace_id}/integrations/cloudflare` | Admin + sudo |
| GET | `/api/workspaces/{workspace_id}/integrations/cloudflare/zones` | Member |
| GET | `/api/workspaces/{workspace_id}/integrations/cloudflare/r2` | Member |
| POST | `/api/workspaces/{workspace_id}/integrations/cloudflare/r2-targets` | Admin |
| GET | `/api/domains/{domain_id}/ingress` | Member |
| PUT | `/api/domains/{domain_id}/ingress` | Member |

See [integrations → Cloudflare](integrations.md#cloudflare).

### Hetzner integration and provisioned servers

| Method | Path | Access |
|---|---|---|
| GET | `/api/workspaces/{workspace_id}/integrations/hetzner` | Viewer |
| PUT | `/api/workspaces/{workspace_id}/integrations/hetzner` | Admin + sudo |
| PATCH | `/api/workspaces/{workspace_id}/integrations/hetzner` | Admin + sudo |
| DELETE | `/api/workspaces/{workspace_id}/integrations/hetzner` | Admin + sudo |
| GET | `/api/workspaces/{workspace_id}/integrations/hetzner/catalog` | Admin |
| GET | `/api/workspaces/{workspace_id}/integrations/hetzner/ssh-keys` | Admin |
| GET | `/api/workspaces/{workspace_id}/servers` | Viewer |
| POST | `/api/workspaces/{workspace_id}/servers/provision` | Admin + sudo |
| GET | `/api/workspaces/{workspace_id}/servers/suggest` | Member |
| GET | `/api/servers/{server_id}` | Viewer |
| DELETE | `/api/servers/{server_id}` | Admin + sudo (`?force=true`) |

See [integrations → Hetzner](integrations.md#hetzner).

### GitHub

| Method | Path | Access |
|---|---|---|
| GET | `/api/github/app` | Auth |
| POST | `/api/github/app/manifest` | Instance admin + sudo |
| GET | `/api/github/app/callback` | Auth (browser redirect) |
| POST | `/api/workspaces/{workspace_id}/github/install` | Admin |
| GET | `/api/github/install/callback` | Auth (browser redirect) |
| GET | `/api/workspaces/{workspace_id}/github/repos` | Member |
| POST | `/api/github/webhook` | Public (HMAC-signed by GitHub) |

## Example: deploy a service from a script

```bash
api() { curl -fsS -H "Authorization: Bearer $MODULA_TOKEN" -H 'content-type: application/json' "$@"; }

# Find the instance of service "api" in "production"
api "$MODULA_URL/api/projects/$PROJECT" | jq -r '
  (.environments[] | select(.name=="production") | .id) as $env
  | (.services[] | select(.name=="api") | .id) as $svc
  | .instances[] | select(.serviceId==$svc and .environmentId==$env) | .id'

# Deploy it and watch the build
api -X POST "$MODULA_URL/api/service-instances/$INSTANCE/deployments" | jq -r .id
```
