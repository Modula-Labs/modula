# Migrating from Railway

Modula Deploy follows Railway's model closely: projects, environments,
services, variables with `${{…}}` references, staged changes, Railpack builds,
`railway.json`/`railway.toml`. Most projects move over without code changes.

## Concepts

| Railway | Modula Deploy |
|---|---|
| Project, environment, service | Same |
| Service instance settings per environment | Same (`config` per service instance) |
| Staged changes / "Deploy" bar | **Deploy N changes** bar ([deployments](deployments.md#pending-changes)) |
| Shared variables `${{shared.X}}` | Same |
| Reference `${{Postgres.DATABASE_URL}}` | `${{postgres.DATABASE_URL}}` — the service's exact name, **lowercase** |
| Private networking `*.railway.internal` | `<service>.modula.internal` (per environment) |
| Railpack / Nixpacks / Dockerfile builders | Railpack (Nixpacks configs build with Railpack) / Dockerfile |
| `railway.json`, `railway.toml` | Read as-is (subset of keys, below); `modula.toml` takes precedence |
| Database templates | PostgreSQL, MySQL, Redis, MongoDB with the same variable names |
| TCP proxy | TCP proxy (ports 20000–29999) |
| Volumes | Volumes (one per service instance) |
| Generated `*.up.railway.app` | Generated `*.<your apps domain>` |
| Custom domains | Custom domains with a TXT ownership record |
| `railway login` / `link` / `connect` | `modula login` / `link` / `connect` |
| Project/account tokens (`RAILWAY_TOKEN`) | API tokens (`MODULA_TOKEN`) |
| Sealed variables | Sealed variables |
| Replicas, cron services, PR environments | Not yet |

## Platform variables

| Railway | Modula Deploy |
|---|---|
| `PORT` | `PORT` (from the service's `port` setting, default 8080) |
| `RAILWAY_PRIVATE_DOMAIN` | `MODULA_PRIVATE_DOMAIN` |
| `RAILWAY_PUBLIC_DOMAIN` | `MODULA_PUBLIC_DOMAIN` |
| `RAILWAY_TCP_PROXY_DOMAIN` / `_PORT` | `MODULA_TCP_PROXY_DOMAIN` / `MODULA_TCP_PROXY_PORT` |
| `RAILWAY_SERVICE_NAME` / `_ID` | `MODULA_SERVICE_NAME` / `MODULA_SERVICE_ID` |
| `RAILWAY_ENVIRONMENT_NAME` / `_ID` | `MODULA_ENVIRONMENT_NAME` / `MODULA_ENVIRONMENT_ID` |
| `RAILWAY_PROJECT_ID` | `MODULA_PROJECT_ID` |
| `RAILWAY_DEPLOYMENT_ID` | `MODULA_DEPLOYMENT_ID` |
| `RAILWAY_DEPLOYMENT_DRAINING_SECONDS` | Honored as-is (or the `drainingSeconds` setting) |
| `RAILWAY_GIT_*`, `RAILWAY_REPLICA_ID`, `RAILWAY_VOLUME_*`, `RAILWAY_STATIC_URL` | No equivalent |

If your code reads a `RAILWAY_*` name, alias it with a reference instead of
changing code:

```
RAILWAY_PUBLIC_DOMAIN  = ${{MODULA_PUBLIC_DOMAIN}}
RAILWAY_PRIVATE_DOMAIN = ${{MODULA_PRIVATE_DOMAIN}}
```

## Config as code

`railway.json` and `railway.toml` are read at the build root. These keys
apply; others are ignored (with no error):

- `build.builder` (`RAILPACK`, `NIXPACKS`, `DOCKERFILE`), `build.buildCommand`,
  `build.dockerfilePath`
- `deploy.startCommand`, `deploy.healthcheckPath`, `deploy.healthcheckTimeout`,
  `deploy.restartPolicyType`, `deploy.restartPolicyMaxRetries`,
  `deploy.preDeployCommand`

Not read: `build.watchPatterns` (set **watch paths** in service settings),
`deploy.numReplicas`, `deploy.cronSchedule`, `deploy.sleepApplication`,
`deploy.region`, and per-environment overrides (`environments.*`).

To manage the whole project as code (Railway's `.railway/railway.ts` with
`railway config plan/apply`), use `.modula/modula.ts`, which has the same
shape (import from `"modula/iac"`), or `.modula/modula.toml`, with
`modula config plan` / `config apply`: services, settings, variables,
domains, volumes and per-environment differences in one file.
`modula init` writes it from an existing project. The key mapping from
`railway.ts` and `railway.toml` is in [config as code → from Railway](config-as-code.md#from-railway).

## Service names

References use the service's name exactly, and names here are lowercase
letters, digits and `-`. Name services as you'll reference them: Railway's
`Postgres` becomes `postgres`, `My API` becomes `my-api`. Then rewrite
references: `${{Postgres.DATABASE_URL}}` → `${{postgres.DATABASE_URL}}`. A
reference with capitals isn't recognized and stays literal text.

## Worked example: a Bun API with Postgres

Railway project `shop`: a Bun API built from a Dockerfile, a Postgres
database, `production` deploying `main` and `staging` deploying `staging`,
`api.example.com` behind Cloudflare.

The repository has:

```json
// railway.json
{
  "$schema": "https://railway.com/railway.schema.json",
  "build": { "builder": "DOCKERFILE", "watchPatterns": ["src/**", "package.json"] },
  "deploy": {
    "preDeployCommand": "bun run migrate",
    "healthcheckPath": "/health",
    "healthcheckTimeout": 60,
    "restartPolicyType": "ON_FAILURE",
    "restartPolicyMaxRetries": 5
  }
}
```

Leave it in place: it's read as-is, except `watchPatterns` (step 3).

### 1. Project and database

Create the project `shop` in the dashboard. Then **+ New → Database →
PostgreSQL**, named `postgres`. Deploy it from the **Deploy changes** bar.

### 2. The API service

**+ New → GitHub repo**: `acme/shop-api`, branch `main`, service name `api`.
In **Settings**:

- Builder: Dockerfile (the file says so too).
- Port: what the app listens on. Make sure Bun binds `0.0.0.0` and reads
  `PORT`: `Bun.serve({ port: Number(process.env.PORT), hostname: "0.0.0.0", … })`.

### 3. Watch paths

`watchPatterns` isn't read from the file. Set **watch paths** in the API's
settings to `src/**` and `package.json`.

### 4. Variables

In the API's **Variables** tab (or the raw editor):

```
DATABASE_URL = ${{postgres.DATABASE_URL}}
NODE_ENV     = production
```

Copy the remaining variables from Railway (`railway variables --kv` prints
them). Mark long-lived production secrets **sealed**. If code reads
`RAILWAY_PUBLIC_DOMAIN`, alias it as shown above.

Over the API:

```bash
curl -fsS -X PATCH -H "Authorization: Bearer $MODULA_TOKEN" -H 'content-type: application/json' \
  "$MODULA_URL/api/service-instances/$API_PROD/variables" -d '{
    "set": { "DATABASE_URL": "${{postgres.DATABASE_URL}}", "NODE_ENV": "production",
             "STRIPE_SECRET_KEY": "sk_live_…" },
    "sealed": ["STRIPE_SECRET_KEY"]
  }'
```

### 5. Copy the data

Put Railway in maintenance (stop writes), then dump from Railway and restore
through a private tunnel. Use a `pg_dump`/`pg_restore` at least as new as both
servers (the template runs Postgres 17).

```bash
# Dump from Railway (its public URL from the Postgres service's variables)
pg_dump --format=custom --no-owner --no-acl "$RAILWAY_DATABASE_PUBLIC_URL" -f shop.dump

# Open a tunnel to the new database (prints a connection string with the password)
modula link --project shop -e production
modula connect postgres --tunnel-only -P 5433
# in another terminal:
pg_restore --no-owner --no-acl --dbname "postgresql://postgres:<password>@127.0.0.1:5433/app" shop.dump
```

Then check it: `modula connect postgres` opens `psql`.

### 6. Deploy

Click **Deploy changes**. The deployment builds the Dockerfile, runs
`bun run migrate` (from `preDeployCommand`) in a one-off container with the
new image, starts the API, waits for `/health` to return 2xx within 60 s, then
switches traffic. If the migration or health check fails, nothing changes for
users.

### 7. Custom domain behind Cloudflare

1. API **Settings → Networking → Generate domain** (the CNAME target), then
   **Custom domain** `api.example.com`.
2. In Cloudflare DNS add, **DNS only** (grey cloud) for now:
   - `CNAME api → api-x7k2.apps.example.com` (the value the dashboard shows)
   - `TXT _modula-challenge.api → modula-verify=…`
3. **Check now** until it's `verified`, and open `https://api.example.com`.
4. Switch the CNAME to **Proxied** and set **SSL/TLS → Full (strict)**.

See [domains](domains.md#behind-cloudflare).

### 8. Staging

1. **New environment** `staging`, cloned from production, with variables
   copied (sealed ones aren't: set them again with staging values).
2. In staging, open the API's settings and set the branch to `staging`.
3. Add a volume to `postgres` in staging at `/var/lib/postgresql/data` (volumes
   aren't cloned), and rotate its password in the dashboard (it needs
   re-authentication) if staging shouldn't share production's.
4. Deploy staging's changes. `${{postgres.DATABASE_URL}}` resolves to staging's
   own database.

Pushes to `main` now deploy production, pushes to `staging` deploy staging.
Later, **Sync from production** copies new settings and variables into staging
(re-check the branch afterwards).

### 9. Cut over

Lower the DNS TTL beforehand, switch DNS (step 7), watch the logs, and keep
the Railway project read-only for a few days before deleting it.

## CLI equivalents

| Railway | Modula Deploy |
|---|---|
| `railway login` | `modula login deploy.example.com` (approve in the browser; or `MODULA_URL` + `MODULA_TOKEN`) |
| `railway logout` / `whoami` | `modula logout` / `whoami` |
| `railway link` / `unlink` | `modula link [--project P] [-e ENV]` / `unlink` |
| `railway connect [service]` | `modula connect [service] [-e ENV]` |
| `railway up [-d] [--ci]` | `modula up [-d] [--ci]`: uploads the directory (honors `.gitignore` and `.modulaignore`) and deploys it |
| `railway logs`, `variables`, `domain`, `redeploy` | The same commands in `modula` (see [CLI](cli.md)) |
