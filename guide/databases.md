# Databases

## Create a database

**+ New → Database** and pick a template. Each one creates a service with the
official image, a volume, a generated 32-character password and ready-made
connection variables, in every environment of the project.

| Template | Image | Port | Volume | Connection variables |
|---|---|---|---|---|
| PostgreSQL (`postgres`) | `postgres:17` | 5432 | `/var/lib/postgresql/data` | `DATABASE_URL`, `DATABASE_PUBLIC_URL`, `PGHOST`, `PGPORT`, `PGUSER`, `PGPASSWORD`, `PGDATABASE` |
| MySQL (`mysql`) | `mysql:8.4` | 3306 | `/var/lib/mysql` | `MYSQL_URL`, `MYSQL_PUBLIC_URL`, `MYSQLHOST`, `MYSQLPORT`, `MYSQLUSER`, `MYSQLPASSWORD`, `MYSQLDATABASE` |
| Redis (`redis`) | `redis:7-alpine` | 6379 | `/data` (append-only) | `REDIS_URL`, `REDIS_PUBLIC_URL`, `REDISHOST`, `REDISPORT`, `REDISUSER`, `REDIS_PASSWORD` |
| MongoDB (`mongo`) | `mongo:8` | 27017 | `/data/db` | `MONGO_URL`, `MONGO_PUBLIC_URL`, `MONGOHOST`, `MONGOPORT`, `MONGOUSER`, `MONGOPASSWORD` |

The default database name is `app` (Postgres, MySQL) and the users are
`postgres`, `root`, `default` and `mongo`.

```bash
curl -fsS -X POST -H "Authorization: Bearer $MODULA_TOKEN" -H 'content-type: application/json' \
  "$MODULA_URL/api/projects/$PROJECT/services" -d '{ "name": "postgres", "template": "postgres" }'
```

Then deploy it (it appears in **Deploy changes**).

## Connect from your services

Reference the database's variables from your app. The private URL uses the
environment's private network (`<service>.modula.internal`), which never leaves
your servers:

```
DATABASE_URL = ${{postgres.DATABASE_URL}}
```

The template variables are themselves built from references
(`postgresql://${{PGUSER}}:${{PGPASSWORD}}@${{MODULA_PRIVATE_DOMAIN}}:5432/${{PGDATABASE}}`),
so they follow renames and proxies. Changing a database variable shows up as a
pending change for every service that references it.

## Connect from your laptop (private tunnel)

The safe default: no public port, just a tunnel through your instance's HTTPS
endpoint, authenticated with your API token.

```bash
modula link                         # once per directory
modula connect                      # Postgres: opens psql
modula connect --tunnel-only -P 5433   # keep a local port open for a GUI client
```

For Postgres, `connect` fetches the credentials and starts `psql` (or prints a
connection string with `--tunnel-only`). For MySQL, Redis and MongoDB it
currently opens the tunnel only: point `mysql`, `redis-cli`, `mongosh` or a GUI
at `127.0.0.1:<port>` with the credentials from the service's variables. See
[CLI → connect](cli.md#connect). Opening a tunnel needs the **Member** role.

## Public access (TCP proxy)

To reach a database from outside (another cloud, a BI tool) enable the
**TCP proxy** in the service's settings. It exposes the service's port on its
server's public address at a port from 20000–29999.

- Host: the instance's `MODULA_TCP_PROXY_HOST` if set, else the server's public
  IP. Port: the allocated one. Both are in `MODULA_TCP_PROXY_DOMAIN` and
  `MODULA_TCP_PROXY_PORT`, and the template's `*_PUBLIC_URL` uses them.
- One TCP proxy per service instance. Open the 20000–29999 range in the
  server's firewall.
- Services that reference the public URL (`${{postgres.DATABASE_PUBLIC_URL}}`)
  show a pending change: deploy them to pick up the proxy's host and port.
- Anyone who can reach the port can try passwords. Prefer `modula
  connect`, and remove the proxy when you're done.

```bash
curl -fsS -X POST -H "Authorization: Bearer $MODULA_TOKEN" -H 'content-type: application/json' \
  "$MODULA_URL/api/service-instances/$INSTANCE/tcp-proxy" -d '{ "targetPort": 5432 }'
```

### TLS

Postgres from the template runs with TLS on: a self-signed certificate is
created on the volume on first start and renewed before it expires.
`DATABASE_PUBLIC_URL` ends in `?sslmode=require` (encrypted, certificate not
verified). The private network also accepts plain connections. The MySQL,
Redis and MongoDB templates don't set up TLS, so their public URLs are
unencrypted: keep them private or use the tunnel.

## Volumes

A volume keeps data across deployments. Each service instance can have one,
mounted at an absolute path such as `/data` (letters, digits, `. _ - /`; not
`/`, `/proc`, `/sys`, `/dev`, `/etc`, `/bin`, `/sbin`, `/lib` or `/usr`). A new
volume takes effect on the next deploy.

Services with a volume deploy with a short interruption: the old container
stops before the new one starts, so two never write the same data.

Deleting a volume deletes its data on the server. It needs the **Admin** role
and a recent re-authentication. Take a [backup](backups.md) first.

## Database tab and API

Postgres services have a **Database** tab: browse tables and rows, edit rows,
run SQL, see stats (connections, cache hit ratio, sizes, unused indexes, top
queries via `pg_stat_statements`), vacuum a table, and install extensions. The
server never connects to your database itself: each request is run by the
server hosting the database, over the environment's private network, with the
database's own credentials.

Every route needs the **Member** role (Viewers and Deployers can't read rows),
works only on services created from the Postgres template, and needs an active
deployment. Every write and every query is written to the audit log (the first
500 characters of the SQL, never results).

| Route | Body / query | Returns |
|---|---|---|
| `GET …/database/tables` | — | `{ tables: [{ schema, name, rowEstimate }] }` |
| `GET …/database/rows` | `?table=public.users&page=1&pageSize=50&orderBy=id&dir=asc` (`pageSize` ≤ 500) | `{ columns, rows, total, page, pageSize, truncated }` |
| `POST …/database/rows` | `{ "table", "values": { col: "text" \| null } }` | `{ rowCount }` |
| `PATCH …/database/rows` | `{ "table", "key": { pk: "…" }, "values": { … } }` | `{ rowCount }` (tables with a primary key) |
| `DELETE …/database/rows` | `{ "table", "key": { pk: "…" } }` | `{ rowCount }` |
| `POST …/database/query` | `{ "sql": "…" }` (≤ 1 MB) | `{ columns, rows, rowCount, command, durationMs, truncated }` |
| `GET …/database/stats` | — | connections, cache hit ratio, sizes, tables, unused indexes, top queries |
| `POST …/database/vacuum` | `{ "table" }` | `{ ok: true }` (`VACUUM (ANALYZE)`) |
| `GET …/database/extensions` | — | `{ installed, available }` |
| `POST …/database/extensions` | `{ "name", "action": "install" \| "uninstall" }` | like GET |
| `GET …/database/credentials` | — | `{ username, password, database, host, port, privateUrl, publicUrl, publicHost, publicPort }` |
| `POST …/database/password` | — | new credentials (needs sudo: dashboard only) |

(`…` is `/api/service-instances/{instance_id}`.)

Query notes:

- The SQL runs as the database's superuser with a 30-second statement
  timeout. The result is the last statement that returned rows (at most 1000
  rows; `truncated` tells you).
- Send `"readOnly": true` (the default for `modula db query`) to run a
  single read statement (`SELECT`, `WITH`, `VALUES`, `TABLE`, `EXPLAIN`,
  `SHOW`) inside a read-only transaction that is always rolled back. Without
  it the SQL runs as a writable script and needs the write permission.

- Postgres errors come back as `400` with Postgres's message.

**Rotate password** runs `ALTER USER` on the live database, stores the new
password in `POSTGRES_PASSWORD`, and marks every service that references the
database as changed: deploy them to pick up the new URL.
