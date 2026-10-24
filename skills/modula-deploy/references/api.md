# Modula Deploy HTTP API reference (for agents)

Base: `$MODULA_URL/api`. Auth: `Authorization: Bearer $MODULA_TOKEN`. JSON,
camelCase, unknown fields rejected. Ids: 16 lowercase base36 chars.

Errors: `{"error":{"code","message"}}` — `bad_request` 400, `unauthorized` /
`invalid_credentials` 401, `forbidden` 403 (role too low / workspace token on
an instance route), `sudo_required` 403 (dashboard-only), `not_found` 404
(also: not visible to you), `conflict` 409, `rate_limited` 429,
`upstream_unavailable` 502.

Roles: Viewer < Deployer < Member < Admin. **+sudo = impossible with a token.**

## Routes

| Method | Path | Min role |
|---|---|---|
| GET | `/auth/me` | any |
| GET | `/workspaces` | any |
| GET/POST | `/workspaces/{ws}/projects` | Viewer / Member |
| GET / PATCH / DELETE | `/projects/{p}` | Viewer / Member / Admin+sudo |
| POST | `/projects/{p}/environments` `{name, cloneFrom?, copyVariables?}` | Member |
| DELETE | `/environments/{env}` | Admin+sudo |
| POST | `/environments/{env}/sync` `{fromEnvironmentId}` | Member |
| GET | `/projects/{p}/activity?environmentId=&serviceId=&kind=` | Viewer |
| GET | `/environments/{env}/config` (whole environment; non-sealed values; audited) | Member |
| GET | `/environments/{env}/pending` | Viewer |
| POST | `/environments/{env}/deploy` `{serviceIds?}` | Deployer |
| POST | `/projects/{p}/services` `{name, config? \| template?}` | Member |
| DELETE | `/services/{s}` | Member+sudo |
| GET / PATCH | `/service-instances/{i}` (`{config}`) | Viewer / Member |
| GET / PATCH | `/service-instances/{i}/variables` | Viewer / Deployer |
| GET / PATCH | `/environments/{env}/variables` (shared) | Viewer / Deployer |
| GET | `/environments/{env}/variable-names` | Viewer |
| POST | `/variables/{v}/reveal` | Member+sudo |
| POST / GET | `/service-instances/{i}/deployments` | Deployer / Viewer |
| GET / DELETE | `/deployments/{d}` | Viewer / Deployer |
| GET | `/deployments/{d}/logs?tail=200&follow=true&kind=runtime\|build` (SSE) | Viewer |
| POST | `/deployments/{d}/redeploy` \| `/rollback` \| `/restart` | Deployer |
| GET / POST | `/service-instances/{i}/domains` (POST = generate) | Viewer / Member |
| POST | `/service-instances/{i}/custom-domains` `{hostname, targetPort?}` | Member |
| POST | `/domains/{d}/check` | Member |
| PATCH / DELETE | `/domains/{d}` (`{targetPort, rateLimitPerMinute}`) | Member |
| GET | `/templates` | any |
| POST | `/service-instances/{i}/volume` `{mountPath}` | Member |
| DELETE | `/volumes/{v}` | Admin+sudo |
| POST | `/service-instances/{i}/tcp-proxy` `{targetPort}` | Member |
| DELETE | `/tcp-proxies/{t}` | Member |
| GET | `/service-instances/{i}/tunnel?port=` (WebSocket) | Member |
| GET | `/service-instances/{i}/database/tables` \| `rows` \| `stats` \| `extensions` \| `credentials` | Member |
| POST | `/service-instances/{i}/database/query` `{sql}` | Member |
| POST/PATCH/DELETE | `/service-instances/{i}/database/rows` | Member |
| POST | `/service-instances/{i}/database/vacuum` `{table}` \| `extensions` `{name, action}` | Member |
| POST | `/service-instances/{i}/database/password` | Member+sudo |
| GET | `/workspaces/{ws}/backup-targets` | Member |
| GET / POST | `/service-instances/{i}/backups` | Viewer / Deployer |
| PUT | `/service-instances/{i}/backup-schedule` `{enabled, targetId, hourUtc, retention?}` | Member |
| POST / DELETE | `/backups/{b}/restore` · `/backups/{b}` | Member+sudo |
| GET / PUT | `/domains/{d}/ingress` (`{proxied}`; Cloudflare-managed zones) | Member |
| GET | `/workspaces/{ws}/integrations/cloudflare` \| `…/zones` \| `…/r2` | Member |
| PUT / DELETE | `/workspaces/{ws}/integrations/cloudflare` (`{token, accountId?}`) | Admin+sudo |
| GET | `/workspaces/{ws}/integrations/hetzner` · `/workspaces/{ws}/servers` · `/servers/{id}` | Viewer |
| GET | `/workspaces/{ws}/servers/suggest?cpuMillis=&memoryMb=&location=` | Member |
| POST | `/workspaces/{ws}/servers/provision` `{location, serverType, name, sshKeyIds, ssh}` | Admin+sudo |
| DELETE | `/servers/{id}?force=` | Admin+sudo |
| GET | `/nodes` | instance admin |
| GET | `/locations` | any |
| GET | `/workspaces/{ws}/github/repos` | Member |

Full table (account, tokens, auth, servers, GitHub App, self-backup): the
guide's `api.md`.

## Shapes worth knowing

- `GET /projects/{p}` → `{id, name, …, environments:[{id,name}], services:[{id,name,template}], instances:[{id, serviceId, environmentId, nodeId, latestDeploymentId, latestDeploymentStatus, domain, sourceType, repo, image, dependsOn, template}]}`.
- `GET /service-instances/{i}` → `{id, serviceName, environmentName, config, status, template, volume, tcpProxy:{publicPort,targetPort,host}, …}`.
- Deployment → `{id, status, trigger, commitSha, commitMessage, source, error, createdAt, …, events:[{phase,message,at}]}`. Status: `building`, `deploying`, `active`, `crashed`, `failed`, `removed`.
- Variables GET → `[{id, name, sealed, references, updatedAt}]` (no values).
- Pending → `{count, services:[{serviceId, serviceName, instanceId, changes}]}`.
- Domain → `{id, hostname, kind, status, dnsRecords:[{type,name,value}], lastError, dnsTargetOk, …}`.
- DB query → `{columns:[{name,type}], rows, rowCount, command, durationMs, truncated}`.
- DB credentials → `{username, password, database, host, port, privateUrl, publicUrl, publicHost, publicPort}` (secret!).

## Service settings (`config`)

| Field | Default | Notes |
|---|---|---|
| `source` | required | `{"type":"image","image":"…"}` or `{"type":"repo","repo":"owner/name","branch":"main","rootDir":"…"}` |
| `startCommand` | image default | argv array, no shell; ignored for Railpack builds |
| `port` | 8080 | injected as `PORT` |
| `healthcheckPath` | none | starts with `/` |
| `healthcheckTimeoutSecs` | 300 | 1–3600 |
| `restartPolicy` | `on_failure` | `on_failure` \| `always` \| `never` |
| `restartPolicyMaxRetries` | none | 1–1000, only with `on_failure` |
| `preDeployCommand` | none | argv run once in the new image before start; failure fails the deploy |
| `drainingSeconds` | 0 | 0–300, SIGTERM→SIGKILL grace |
| `memoryMb` / `cpuMillis` | 512 / 1000 | 16–1048576 / 10–256000 |
| `replicas` | 1 | only 1 accepted |
| `builder` | `railpack` | `railpack` \| `dockerfile` (repo sources) |
| `dockerfilePath` | `Dockerfile` | relative to root dir |
| `buildCommand` | detected | ≤4096 bytes |
| `autoDeploy` | true | deploy on push |
| `watchPaths` | [] | gitignore-style globs, ≤50 |

A `modula.toml` / `railway.toml` / `railway.json` in the repo overrides
build/deploy keys per build (builder, buildCommand, dockerfilePath,
startCommand, healthcheckPath, healthcheckTimeout, restartPolicyType,
restartPolicyMaxRetries, preDeployCommand).
