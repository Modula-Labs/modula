# Changelog

All notable changes to Modula Deploy are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) (pre-1.0: minor versions
may break things).

## [Unreleased]

## [0.1.0] - 2026-10-24

First public release: a self-hosted PaaS with Railway's project canvas,
environments and staged deploys, licensed AGPL-3.0-only.

### Added

- **Install:** one command on Ubuntu; the dashboard is served on
  `https://<public IP>` with a Let's Encrypt IP certificate. Dashboard and apps
  domains are set later in Settings → Domain (or with the CLI).
- **Projects:** canvas with services, databases and volumes; environments;
  staged changes deployed together; project settings, activity and logs.
- **Sources:** GitHub repositories (one private GitHub App per account or
  organization, push-to-deploy, commit statuses), Docker images, and local
  files uploaded with `modula up`. Builds with Railpack or a Dockerfile in a
  sandboxed, rootless BuildKit per workspace.
- **Variables:** per service and shared per environment, `${{Service.VAR}}`
  references, sealed at rest, raw `.env` editor.
- **Networking:** generated and custom domains with automatic HTTPS, TCP
  proxies, Cloudflare DNS integration, and a private network between servers
  (WireGuard mesh, per-environment isolation, `<service>.modula.internal`).
- **Scaling:** several servers, replicas spread one per server with load
  balancing, CPU and memory limits, moving a service between servers, and
  Hetzner Cloud server provisioning.
- **Databases:** Postgres, MySQL, Redis and MongoDB templates; a Database tab;
  scheduled backups to S3-compatible storage with restore.
- **Observability:** stored deployment and runtime logs with search; CPU, memory
  and network metrics; outgoing deployment webhooks.
- **Security:** sign-in with TOTP and sudo for sensitive actions; roles per
  workspace; signed releases and installers; a private dashboard behind
  Cloudflare Tunnel and Access.
- **CLI:** `modula` for projects, deployments, logs, variables, metrics,
  `modula.toml` infrastructure as code, and `modula server create`.

[Unreleased]: https://github.com/Modula-Labs/modula/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Modula-Labs/modula/releases/tag/v0.1.0
