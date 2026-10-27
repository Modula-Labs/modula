# Environments

Every project starts with `production`. Add environments such as `staging` to
run a full copy of the project: each service has its own settings, variables,
domains, volume and deployments in every environment it's in. A new service is
added to every environment; deleting one from an environment removes it from
that environment only (see
[delete a service](projects-and-services.md#delete-a-service)). Environments are
isolated: each has its own private network and its own encryption key, and
services in one can't reach services in another.

## Create an environment

**Environment menu (top bar) → New environment**, or **Project Settings →
Environments**. Start from:

- **Duplicate <environment>**: every service's settings and all its variables,
  service and shared. Sealed variables are copied server-side and stay sealed.
- **Empty**: every service with the default environment's settings and no
  variables.

Either way:

- **Branch.** Pick the branch each GitHub service deploys from in the new
  environment (default: the same branch). Pushes to it deploy here only.
- **Databases** get a new, empty volume at the same path and freshly generated
  credentials; their data is never copied.
- **R2 buckets** and their credentials are not copied: create or link a bucket
  in the new environment (the service's Bucket tab).
- **Domains and TCP proxies** are not copied: generate or add them for the new
  environment. Generated names include the environment
  (`api-staging-x7k2.apps.example.com`); the default environment's don't.

Names are stored lowercase and must be unique in the project.

```bash
curl -fsS -X POST -H "Authorization: Bearer $MODULA_TOKEN" -H 'content-type: application/json' \
  "$MODULA_URL/api/projects/$PROJECT/environments" \
  -d '{ "name": "staging", "duplicateFrom": "<production env id>", "branches": { "<service id>": "staging" } }'
```

Nothing is deployed yet: the new environment's services show up in the
**Deploy changes** bar.

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

## Switch, rename and delete

The environment menu in the top bar switches environments and keeps you on the
same page; the choice is in the URL (`?env=`) and remembered per project.

Rename an environment in **Project Settings → Environments** (or
`PATCH /api/environments/{id}` with `{ "name": "…" }`); existing generated
domains keep their hostnames.

Deleting removes its deployments, volumes (with their data), variables and
encryption key. The default environment (the project's first, `production`)
can't be deleted. Needs the **Admin** role, typing the environment's name, and
a recent re-authentication (dashboard only).

## PR environments (coming soon)

Per-pull-request environments are planned but not available yet.
