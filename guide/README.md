# Modula Deploy guide

Modula Deploy is a self-hosted platform with Railway's workflow: push to
GitHub or pick an image, and it builds, deploys, routes and secures your
services and databases on your own servers.

## Get started

1. [Install](install.md) the main server and create your admin account.
2. Create a project and add services: [projects and services](projects-and-services.md).
3. Add a database and connect your app: [databases](databases.md), [variables](variables.md).
4. Deploy, watch logs, roll back: [deployments](deployments.md).
5. Give it a domain: [domains](domains.md).

## Guides

| Page | What's in it |
|---|---|
| [Install](install.md) | Requirements, ports, install, recovery kit, configuration, upgrades |
| [Servers](servers.md) | Adding servers, locations, placement, removal |
| [Projects and services](projects-and-services.md) | The canvas, sources, builders, config as code, every service setting |
| [Variables](variables.md) | Service and shared variables, references, platform variables, sealed variables |
| [Environments](environments.md) | Clone, sync and delete environments |
| [Deployments](deployments.md) | Pending changes, rollout, health checks, pre-deploy, draining, redeploy/rollback/restart, logs |
| [Databases](databases.md) | Templates, volumes, private tunnels, TCP proxies, TLS, the Database tab |
| [Backups](backups.md) | Encrypted scheduled backups to S3, R2, B2 or MinIO, and restores |
| [Domains](domains.md) | Generated and custom domains, DNS records, Cloudflare |
| [Integrations](integrations.md) | GitHub, Cloudflare and Hetzner |
| [Config as code](config-as-code.md) | `modula.toml`: the whole project in git, `config plan` / `config apply` |
| [CLI](cli.md) | Every `modula-deploy` command and flag |
| [API](api.md) | Tokens, errors, roles and every route |
| [Security](security.md) | 2FA, sudo mode, API tokens, roles, secrets, audit log |
| [Migrating from Railway](migrating-from-railway.md) | Concept and variable mapping, a full worked migration |

## For AI agents

Agent skills live in [`skills/`](../skills): `modula-deploy` (operate apps
through the CLI and API) and `migrate-from-railway` (port a Railway project).
