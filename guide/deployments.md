# Deployments

A deployment is an immutable snapshot of a service's settings and resolved
variables, placed on one server. Its status moves through:

```
queued → building → deploying → active ──▶ crashed ──▶ active
              │          │          │
            failed     failed     removed (replaced by a newer deployment)
```

Image deployments start at `deploying`; repository and upload deployments at
`building`.

## What triggers a deployment

- **A push** to the service's branch, when `autoDeploy` is on and the push
  touches one of `watchPaths` (if set). Each environment's service instance has
  its own branch, so `main` can deploy production and `staging` staging.
- **Deploy** on the service (`POST /api/service-instances/{id}/deployments`):
  the current settings and variables, and the branch's latest commit.
- **Deploy changes** in the pending-changes bar (below).
- **Redeploy** and **Rollback** on a past deployment.
- **`modula-deploy up`**: files uploaded from your machine (below).

## Uploaded sources (`modula-deploy up`)

`up` deploys local files without GitHub. The CLI packs the directory
(honoring `.gitignore` and `.modulaignore`; never `.git/` or `.modula/`),
uploads it, and deploys it. The deployment builds those files exactly like a
repository checkout: the service's builder, root directory, build and start
commands, and any `railway.toml` / `railway.json` / `modula.toml` in them.
The service's configured source is not changed; the deployment is listed as
**Uploaded via CLI**, with the local branch, commit and "uncommitted changes"
when the CLI found a Git checkout.

The API, for other clients:

1. `POST /api/service-instances/{id}/uploads` with the gzipped tarball as the
   body (`Content-Type: application/gzip`; optional `x-content-sha256` with
   its hex SHA-256, rejected on mismatch). The archive must hold one
   top-level directory (it is stripped, like GitHub tarballs). Returns
   `{ id, sha256, sizeBytes }`. Needs the Deployer role (like deploying); 60 uploads per user per
   hour, 2 at a time per workspace, `413` over the size limit
   (`MODULA_UPLOAD_MAX_MB`, default 200).
2. `POST /api/service-instances/{id}/deployments` with
   `{ "source": { "upload": "<id>" }, "message": "…", "git": { "commit", "branch", "subject", "dirty" } }`
   (`message` and `git` optional, display only). The deployment's `trigger`
   is `upload` and its `sourceKind` `upload`.

Uploads are never downloadable by clients. The server that builds the
deployment downloads the archive from the server with a short-lived token
bound to that deployment (valid only while it is building, for at most 6
hours), verifies its
SHA-256 and unpacks it safely (no absolute paths, `..`, links leaving the
tree, devices or hard links; size and file-count limits).

**Retention.** Archives are stored on the server's disk
(`MODULA_UPLOADS_DIR`, default `uploads/` next to the master key; with several
server replicas, use a shared volume). An upload is deleted 24 hours after it
was made unless a running deployment or one of the service's 5 newest
deployments uses it; until then **Redeploy** and **Rollback** rebuild it
(`uploadAvailable` on the deployment). After that, upload again. Nodes reach
the server at `MODULA_INTERNAL_URL` (default `MODULA_PUBLIC_URL`) to download.

## Pending changes

Editing settings or variables never touches what's running. Each edit is
recorded, and the **Deploy N changes** bar lists every service with changes
since its last deployment (a shared variable's change counts for every service
that references it).

- **Deploy** deploys all of them, or pick services.
  API: `GET /api/environments/{id}/pending`, then
  `POST /api/environments/{id}/deploy` with `{ "serviceIds": [...] }` or `{}`.
- Services deploy independently: one failing to start doesn't stop the others.
  The response lists `{ serviceId, deploymentId, error }` per service.

## The rollout

For each new deployment the server it runs on:

1. **Builds** (repository and upload sources): fetches the commit (or the
   uploaded files) and builds with Railpack or your Dockerfile. Build output is
   in the build log.
2. **Pulls** the image (image sources).
3. Runs the **pre-deploy command**, if set (below).
4. **Starts** the new container next to the old one.
5. **Health-checks** it (below).
6. Marks it **active**, routes traffic to it, and retires the previous
   deployment.

Services with a volume (databases) are the exception: two containers must never
share the volume's data, so the old container is stopped before the new one
starts, which means a short interruption.

### Health checks

With a `healthcheckPath` (e.g. `/health`), the new container must answer that
path on its `port` with a 2xx status within `healthcheckTimeoutSecs` (default
300). If it doesn't, the deployment fails, its container is removed, and the
previous deployment keeps serving. Without a health check, the container only
has to stay up for 2 seconds.

### Pre-deploy command

`preDeployCommand` (an argv list in the dashboard/API, or a shell string or
list of strings in `modula.toml`/`railway.toml`/`railway.json`) runs once, in a
one-off container from the new image with the deployment's variables, before
the new deployment starts. Use it for database migrations:

```json
{ "preDeployCommand": ["bun", "run", "migrate"] }
```

- Its output goes to the build log.
- It has 10 minutes. A non-zero exit fails the deployment and the previous one
  keeps serving.

### Draining

When a deployment is retired, its container gets SIGTERM, then SIGKILL after
`drainingSeconds` (0–300; 0 = the server's default). Finish in-flight requests
on SIGTERM. A `RAILWAY_DEPLOYMENT_DRAINING_SECONDS` variable on the service
overrides the setting.

### Restart policy

`restartPolicy` decides what happens when the process exits: `on_failure`
(default; optionally capped by `restartPolicyMaxRetries`, 1–1000), `always`, or
`never`. A crash marks the deployment `crashed`; it returns to `active` when the
container recovers.

## Deployment actions

| Action | What it does | Works on |
|---|---|---|
| **Redeploy** | Deploys the same source again: the same image, the same commit rebuilt, or the same upload rebuilt. Settings come from that deployment's snapshot; variables are the current ones. | Any deployment (uploads: while kept) |
| **Rollback** | Rebuilds and redeploys that deployment's commit or upload. Repository code is never stored, so a rollback is always a fresh build of the commit. | Deployments built from a Git commit, or from an upload that is still kept (use Redeploy for images) |
| **Restart** | Restarts the running container in place (same deployment, same image). | `active` or `crashed` deployments |
| **Remove** | Stops the deployment and removes its container. | Running deployments (`queued` … `crashed`) |

API: `POST /api/deployments/{id}/redeploy`, `/rollback`, `/restart`, and
`DELETE /api/deployments/{id}`. All need the **Deployer** role.

## Logs

The deployment sheet shows build and runtime logs live. Secret values (and
their base64 and URL-encoded forms) are masked.

```bash
# The last 500 runtime lines, without following
curl -N -H "Authorization: Bearer $MODULA_TOKEN" \
  "$MODULA_URL/api/deployments/$DEP/logs?tail=500&follow=false"

# The build log
curl -N -H "Authorization: Bearer $MODULA_TOKEN" \
  "$MODULA_URL/api/deployments/$DEP/logs?kind=build&follow=false"
```

Query parameters: `tail` (default 200, max 5000), `follow` (default `true`),
`kind` (`runtime` or `build`). The stream is Server-Sent Events: `log` events
with `{ ts, stderr, text }`, then `end`. Logs come from the server running the
deployment, so they're unavailable (`409`) while it's offline.

`GET /api/deployments/{id}` returns the deployment with its `events` (phase
changes such as `healthchecking`, with messages) and `error` when it failed.
`GET /api/service-instances/{id}/deployments` lists the newest 50.

## Troubleshooting

| Symptom | Look at |
|---|---|
| `failed` during build | Build log (`kind=build`). Check the builder, root directory and Dockerfile path. |
| `Health check /x did not pass within Ns` | Does the app listen on `0.0.0.0:$PORT`? Is the path right? Raise `healthcheckTimeoutSecs` for slow starts. |
| `Pre-deploy command exited with code N` | The error shows the tail of its output; the full output is in the build log. |
| `crashed` | Runtime log; the restart policy may be retrying. |
| Variables empty at runtime | A reference to a missing variable or service resolves to `""`. Check names (service names are lowercase). |
| Change didn't apply | Settings and variables only apply on the next deployment: use **Deploy changes**. |
