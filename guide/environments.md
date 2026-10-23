# Environments

Every project starts with `production`. Add environments such as `staging` to
run a full copy of the project: every service exists in every environment, with
its own settings, variables, domains, volume and deployments. Environments are
isolated: each has its own private network and its own encryption key, and
services in one can't reach services in another.

## Create an environment

**Environment menu → New environment**. Options:

- **Clone from**: the environment whose service settings to copy (default: the
  project's first environment).
- **Copy variables**: also copy its service and shared variables. Sealed
  variables are never copied; set them again in the new environment.

Names are stored lowercase and must be unique in the project.

```bash
curl -fsS -X POST -H "Authorization: Bearer $MODULA_TOKEN" -H 'content-type: application/json' \
  "$MODULA_URL/api/projects/$PROJECT/environments" \
  -d '{ "name": "staging", "cloneFrom": "<production env id>", "copyVariables": true }'
```

Nothing is deployed yet: the new environment's services show up in the
**Deploy changes** bar. Before deploying:

- **Branch.** For GitHub services, set the branch for this environment in each
  service's settings (e.g. `staging`); pushes to it deploy here only.
- **Databases.** Volumes, domains and TCP proxies are not cloned. Add a volume
  to each database service in the new environment (Settings → Volume) at its
  template's path before its first deploy: `/var/lib/postgresql/data`
  (Postgres), `/var/lib/mysql` (MySQL), `/data` (Redis), `/data/db` (MongoDB).
  The new database starts empty.
- **Credentials.** With *Copy variables*, database passwords are copied too;
  rotate them if environments shouldn't share credentials. Without it, set the
  database's variables yourself.
- **Domains.** Generate or add domains for the new environment. Generated
  names include the environment (`api-staging-x7k2.apps.example.com`).

References like `${{postgres.DATABASE_URL}}` resolve inside each environment, so
staging's API talks to staging's database without changes.

## Sync environments

**Sync from…** brings an environment in line with another of the same project:

- every service's settings are copied from the source,
- variables the target lacks are added (existing values are kept, sealed
  variables are never copied).

The changes take effect on each service's next deployment, through the
**Deploy changes** bar.

```bash
curl -fsS -X POST -H "Authorization: Bearer $MODULA_TOKEN" -H 'content-type: application/json' \
  "$MODULA_URL/api/environments/$STAGING/sync" -d '{ "fromEnvironmentId": "<production env id>" }'
```

Sync copies settings wholesale, including each service's branch. Re-check
branches after syncing staging from production.

## Delete an environment

Deletes its deployments, volumes (with their data), variables and encryption
key. A project keeps at least one environment. Needs the **Admin** role and a
recent re-authentication (dashboard only).

## PR environments (coming soon)

Per-pull-request environments are planned but not available yet.
