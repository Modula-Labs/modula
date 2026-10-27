# Variables reference (for agents)

## Write

```bash
# service variables (Deployer)
api -X PATCH "$MODULA_URL/api/service-instances/$I/variables" \
  -d '{"set":{"NODE_ENV":"production","DATABASE_URL":"${{postgres.DATABASE_URL}}"},"sealed":[],"delete":["OLD"]}'
# shared variables of an environment
api -X PATCH "$MODULA_URL/api/environments/$ENV/variables" -d '{"set":{"SENTRY_DSN":"…"}}'
```

- `set` upserts, `delete` removes (a name can't be in both), `sealed` lists
  names from `set` to store sealed.
- Names: `[A-Za-z_][A-Za-z0-9_]*`, ≤255 chars, not starting with `MODULA_`.
- Values ≤32 KB, no NUL. ≤500 variables per service / per environment.
- Shell-quote values in single quotes so `${{…}}` isn't expanded by bash.
- Changes are staged: they apply on the next deployment of each affected
  service (shared-variable changes affect every service referencing them).

## Read

- `GET …/variables` returns names, `sealed`, `references`, `updatedAt`. Never
  values.
- `GET /api/environments/{env}/variable-names` lists every referenceable name:
  `shared` and per service `names` + `platform`.
- Values can only be revealed in the dashboard (Member + sudo). Sealed ones
  never. Don't ask the user to paste values into chat; ask them to set them.

## References

| Syntax | Means |
|---|---|
| `${{NAME}}` | same service's variable (inside a shared variable: another shared one) |
| `${{shared.NAME}}` | shared variable of the environment |
| `${{svc.NAME}}` | variable of service `svc` in the same environment |

- `svc` is the exact service name: lowercase, digits, `-`. `${{Postgres.X}}` is
  NOT a reference (kept literally).
- Platform variables of any service are referenceable:
  `${{api.MODULA_PUBLIC_DOMAIN}}`, `${{postgres.MODULA_PRIVATE_DOMAIN}}`.
- Resolved when a deployment is created; missing targets become `""`.
- Cycles are rejected on save (`400`); max depth 16.
- Shared variables are not injected automatically; reference them.

## Platform variables (injected into every deployment)

`PORT` (the `port` setting; a user `PORT` wins), `MODULA_PRIVATE_DOMAIN`
(`<service>.modula.internal`), `MODULA_PUBLIC_DOMAIN` (first domain or empty),
`MODULA_SERVICE_NAME`, `MODULA_SERVICE_ID`, `MODULA_ENVIRONMENT_NAME`,
`MODULA_ENVIRONMENT_ID`, `MODULA_PROJECT_ID`, `MODULA_TCP_PROXY_DOMAIN`,
`MODULA_TCP_PROXY_PORT`, and `MODULA_DEPLOYMENT_ID` (injected, not
referenceable).

`RAILWAY_DEPLOYMENT_DRAINING_SECONDS` set on a service overrides its
`drainingSeconds`.

## Database template variables

| Template | Use from apps |
|---|---|
| `postgres` | `${{postgres.DATABASE_URL}}` (private), `DATABASE_PUBLIC_URL` (TCP proxy, `sslmode=require`), `PGHOST`, `PGPORT`, `PGUSER`, `PGPASSWORD`, `PGDATABASE` |
| `mysql` | `MYSQL_URL`, `MYSQL_PUBLIC_URL`, `MYSQLHOST`, `MYSQLPORT`, `MYSQLUSER`, `MYSQLPASSWORD`, `MYSQLDATABASE` |
| `redis` | `REDIS_URL`, `REDIS_PUBLIC_URL`, `REDISHOST`, `REDISPORT`, `REDISUSER`, `REDIS_PASSWORD` |
| `mongo` | `MONGO_URL`, `MONGO_PUBLIC_URL`, `MONGOHOST`, `MONGOPORT`, `MONGOUSER`, `MONGOPASSWORD` |

(Prefix with the service's actual name, e.g. `${{postgres.DATABASE_URL}}`.)
Public URLs are empty-hosted until a TCP proxy exists; prefer the private URL.

## Sealed

Permanent, injected normally, never revealable, never copied by environment
clone/sync. Ask the user before sealing.
