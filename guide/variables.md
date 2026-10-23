# Variables

Variables become environment variables in your service's container. They are
encrypted at rest with a key per environment, write-only over the API, and
masked in deploy logs.

## Service and shared variables

- **Service variables** belong to one service in one environment
  (**Variables** tab of the service).
- **Shared variables** belong to an environment and are available to every
  service by reference (`${{shared.NAME}}`), in **Project settings → Shared
  variables**. They are not injected automatically; reference the ones a
  service needs.

Rules:

- Names: letters, digits and `_`, not starting with a digit, at most 255
  characters. Names starting with `MODULA_` are reserved.
- Values: up to 32 KB, no NUL bytes.
- At most 500 variables per service (and per environment for shared ones).

Changing a variable doesn't restart anything. The change appears in the
**Deploy changes** bar and takes effect on the next deployment of each affected
service. A shared variable's change counts for every service that references it.

Over the API (needs **Deployer**):

```bash
curl -fsS -X PATCH -H "Authorization: Bearer $MODULA_TOKEN" -H 'content-type: application/json' \
  "$MODULA_URL/api/service-instances/$INSTANCE/variables" -d '{
    "set":    { "NODE_ENV": "production", "DATABASE_URL": "${{postgres.DATABASE_URL}}" },
    "sealed": [],
    "delete": ["OLD_FLAG"]
  }'
```

`GET` on the same path returns names, `sealed`, `references` and `updatedAt`,
never values.

## References

A value can include other variables with `${{ … }}`:

| Syntax | Refers to |
|---|---|
| `${{NAME}}` | Another variable of the same service (or, inside a shared variable, another shared variable). |
| `${{shared.NAME}}` | A shared variable of the environment. |
| `${{service.NAME}}` | A variable of another service in the same environment, e.g. `${{postgres.DATABASE_URL}}`. |

```
DATABASE_URL = ${{postgres.DATABASE_URL}}
REDIS_URL    = ${{redis.REDIS_URL}}
API_URL      = https://${{api.MODULA_PUBLIC_DOMAIN}}
SENTRY_DSN   = ${{shared.SENTRY_DSN}}
```

- The service part is the service's name exactly: lowercase letters, digits
  and `-`. `${{Postgres.DATABASE_URL}}` (capitalized) is not a reference and is
  kept as literal text.
- References may point at platform variables of any service (below).
- References are resolved when a deployment is created; a deployment keeps the
  values it started with. A reference to a variable that doesn't exist becomes
  an empty string.
- References that loop (`A → B → A`) are rejected when you save. Chains are
  followed up to 16 levels deep.
- In the dashboard, the canvas draws an edge from a service to every service it
  references, directly or through a shared variable.

## Platform variables

Every deployment gets these. All but `MODULA_DEPLOYMENT_ID` can also be
referenced from other services (`${{api.MODULA_PUBLIC_DOMAIN}}`).

| Variable | Value |
|---|---|
| `PORT` | The service's `port` setting (default 8080). A `PORT` variable you set wins. |
| `MODULA_PRIVATE_DOMAIN` | `<service>.modula.internal`: reach the service from others in the same environment. |
| `MODULA_PUBLIC_DOMAIN` | The service's first public domain (empty if none). |
| `MODULA_SERVICE_NAME` | Service name. |
| `MODULA_SERVICE_ID` | Service id. |
| `MODULA_ENVIRONMENT_NAME` | Environment name, e.g. `production`. |
| `MODULA_ENVIRONMENT_ID` | Environment id. |
| `MODULA_PROJECT_ID` | Project id. |
| `MODULA_DEPLOYMENT_ID` | This deployment's id (not referenceable). |
| `MODULA_TCP_PROXY_DOMAIN` | Public host of the service's TCP proxy (empty if none). |
| `MODULA_TCP_PROXY_PORT` | Public port of the service's TCP proxy (empty if none). |

`RAILWAY_DEPLOYMENT_DRAINING_SECONDS`, if you set it on a service, overrides the
`drainingSeconds` setting (0–300), so Railway apps keep their setting.

## Sealed variables

Mark a variable **sealed** when you set it (`"sealed": ["NAME"]` in the API).
A sealed variable:

- is injected into deployments like any other,
- can never be revealed again, by anyone, through the dashboard or the API,
- stays sealed forever (setting a new value keeps it sealed),
- is never copied when an environment is cloned or synced.

Use it for production secrets nobody needs to read back.

## Revealing values

The dashboard shows a variable's value only on **Reveal**, which needs the
**Member** role and a recent re-authentication, and is written to the audit
log. API tokens can't reveal values. Sealed variables can't be revealed.
