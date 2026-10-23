---
name: migrate-from-railway
description: Use when porting a Railway project to Modula Deploy - reading railway.json / railway.toml / .railway config, mapping Railway services, settings and RAILWAY_* variables, recreating variables with ${{service.VAR}} references, and moving Postgres data with pg_dump/pg_restore through a modula-deploy tunnel. Pair with the modula-deploy skill for the API and CLI details.
---

# Migrate a Railway project to Modula Deploy

Use the `modula-deploy` skill for auth, ids and API calls. This skill is the
migration plan. Work service by service, staging before production, and
confirm each step that changes the target or touches data.

## 1. Inventory the Railway side (read-only)

- Repo files: `railway.json`, `railway.toml`, anything under `.railway/`,
  `Dockerfile`, `nixpacks.toml`, `package.json` scripts, `Procfile`.
- `railway status`, `railway variables --kv` per service and environment (ask
  the user to run these if you lack access; **don't paste values into chat**:
  have them write to a local file that you read, then delete it).
- For each service note: name, source (repo+branch per environment, or image),
  root directory, builder, start/build commands, port, health check, restart
  policy, pre-deploy command, watch patterns, volumes (mount path), domains,
  TCP proxies, replicas, cron schedule, and which variables are references.

## 2. Map

| Railway | Modula Deploy |
|---|---|
| Service `Postgres`, `My API` | Rename to `postgres`, `my-api` (lowercase, digits, `-`; ≤63) |
| `${{Postgres.DATABASE_URL}}` | `${{postgres.DATABASE_URL}}` (capitalized refs are NOT parsed) |
| `${{shared.X}}`, `${{X}}` | Same |
| `RAILWAY_PRIVATE_DOMAIN` | `MODULA_PRIVATE_DOMAIN` (`<svc>.modula.internal`) |
| `RAILWAY_PUBLIC_DOMAIN` | `MODULA_PUBLIC_DOMAIN` |
| `RAILWAY_TCP_PROXY_DOMAIN` / `_PORT` | `MODULA_TCP_PROXY_DOMAIN` / `_PORT` |
| `RAILWAY_SERVICE_NAME`/`_ID`, `RAILWAY_ENVIRONMENT_NAME`/`_ID`, `RAILWAY_PROJECT_ID`, `RAILWAY_DEPLOYMENT_ID` | `MODULA_*` equivalents |
| `RAILWAY_DEPLOYMENT_DRAINING_SECONDS` | Honored as-is |
| `RAILWAY_GIT_*`, `RAILWAY_REPLICA_ID`, `RAILWAY_VOLUME_*`, `RAILWAY_STATIC_URL` | None — find usages in code |
| Database plugins/templates | Templates `postgres`, `mysql`, `redis`, `mongo` (same variable names) |
| `numReplicas` > 1, cron services, sleeping, regions, PR envs | Not supported yet — tell the user |

Variables Railway injects that the app reads (`RAILWAY_*`) can be aliased
without code changes: `RAILWAY_PUBLIC_DOMAIN = ${{MODULA_PUBLIC_DOMAIN}}`.
User variables may not start with `MODULA_`.

### Config as code

`railway.json`/`railway.toml` stay in the repo and are read per build. Keys
used: `build.builder` (RAILPACK/NIXPACKS → Railpack, DOCKERFILE),
`build.buildCommand`, `build.dockerfilePath`, `deploy.startCommand`,
`deploy.healthcheckPath`, `deploy.healthcheckTimeout`,
`deploy.restartPolicyType`, `deploy.restartPolicyMaxRetries`,
`deploy.preDeployCommand`. Ignored: `build.watchPatterns` (set `watchPaths`
in the service config instead), `deploy.numReplicas`, `deploy.cronSchedule`,
`deploy.sleepApplication`, `environments.*` overrides (apply them per
environment in the service config by hand). `.railway/*.ts` IaC is not read:
translate it to API calls.

## 3. Recreate

1. Project in the right workspace (Member).
2. Databases first: `{"name":"postgres","template":"postgres"}`; deploy them.
3. App services with `config` from the inventory (`source`, `builder`, `port`,
   `healthcheckPath`, `preDeployCommand` as an argv array, `watchPaths`,
   `rootDir`). Leave settings that `railway.json` already sets to the file.
4. Variables: copy literals; rewrite references to lowercase service names;
   replace Railway-only values (e.g. `*.railway.internal` hosts) with
   references (`${{api.MODULA_PRIVATE_DOMAIN}}`). Ask before sealing.
5. Environments: create `staging` with `cloneFrom` production and
   `copyVariables`, then per environment: set each repo service's `branch`, add
   a volume to each database at its template path
   (`/var/lib/postgresql/data`, `/var/lib/mysql`, `/data`, `/data/db`) — volumes
   aren't cloned — and set sealed variables again.
6. Deploy pending changes per environment; check every service reaches
   `active` and logs are clean.

## 4. Move Postgres data

Confirm a maintenance window with the user (writes on Railway must stop, or
data written after the dump is lost).

```bash
# Source: Railway's DATABASE_PUBLIC_URL (from the user; keep it in an env var)
pg_dump --format=custom --no-owner --no-acl "$RAILWAY_DB_URL" -f /tmp/railway.dump

# Target: private tunnel, no public port
modula-deploy connect postgres -e production --tunnel-only -P 54329 > /tmp/tunnel.log 2>&1 &
TUNNEL=$!; sleep 2
TARGET=$(sed -n 's/^Connection string: //p' /tmp/tunnel.log)   # has the password; never print
pg_restore --no-owner --no-acl --exit-on-error --dbname "$TARGET" /tmp/railway.dump
psql "$TARGET" -X -c 'SELECT count(*) FROM <a key table>'        # compare with the source
kill $TUNNEL; rm -f /tmp/tunnel.log /tmp/railway.dump
```

- Client tools must be at least the newer server's major version (target runs
  Postgres 17).
- Restore into an empty database. If the app's pre-deploy migration already
  created tables, ask before dropping them (or restore before the first app
  deploy).
- MySQL: `mysqldump` → `mysql` through `connect mysql --tunnel-only`
  (credentials from the service's variables, set by the user). Redis/Mongo:
  `redis-cli --rdb` / `mongodump` + `mongorestore` the same way.

## 5. Domains and cut-over

1. Generate a domain per public service and test it.
2. Custom domains: create them, give the user the `dnsRecords` (CNAME/A +
   `TXT _modula-challenge.<host>`). Behind Cloudflare: DNS-only until
   `verified` and serving HTTPS, then proxied with SSL Full (strict).
3. Lower TTLs first; switch DNS; watch logs; keep Railway read-only for a few
   days. Deleting the Railway project is the user's call.

## Checklist to report back

- Services created and their status per environment.
- Variables that still need values from the user (names only).
- Railway features without an equivalent and what you did instead.
- Data migration row-count check results.
- DNS records the user must create.
