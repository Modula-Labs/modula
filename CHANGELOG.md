# Changelog

All notable changes to Modula Deploy are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) (pre-1.0: minor versions
may break things).

## [Unreleased]

### Added

- **Browser login for the CLI:** `modula login` asks for the server, opens the
  dashboard to approve the login (compare the code, approve with a recent
  sign-in) and saves a token named `CLI on <machine>`. `--browserless` prints
  the URL; `--token`/`MODULA_TOKEN` remain for CI.
- **Several instances at once:** logins are saved under a local name
  (`--name`); `modula login list`, `modula login use`, `--login <name>` /
  `MODULA_LOGIN`, `modula logout [<name>|--all]`. A linked directory keeps
  using the login it was linked with.
- **CLI pickers:** at a terminal, a missing or ambiguous project, service,
  workspace, database, deployment (`rollback`), member or login is chosen
  from a list instead of failing.
- **Config as code in TypeScript:** `.modula/modula.ts`, shaped like
  Railway's `railway.ts` (`import { project, service, github } from
  "@modula-labs/iac"`, `ctx.isEnvironment(...)`, `ctx.isPreview`). The CLI runs it
  with Node 22.18+ or Bun; nothing to install.
- **`modula init` like `railway init`:** pick the workspace, create a
  project or link an existing one, pick the environment and write
  `.modula/modula.ts` from the project (`partial` when the repository holds
  only some of its services). Flags (`-p`, `-e`, `--format`, `--yes`) for CI.

### Changed

- The CLI config file holds named logins (`current` plus `[logins.<name>]`);
  `modula login` takes the URL as an argument instead of `--url`.
- Config files live in `.modula/` (`modula.ts`, `modula.toml` or
  `modula.json`); a root `modula.toml` is only the build-time file now.
  `.modula/.gitignore` keeps the link, token and secrets out of git instead
  of the repository ignoring all of `.modula/`.

## [0.1.1] - 2026-10-25

### Added

- **Buckets:** add an S3-compatible bucket to a project, either on Cloudflare
  R2 (Modula creates the bucket and a token limited to it) or self-hosted on
  your servers. Credentials become the service's variables.
- **Static sites on Cloudflare:** a service's Hosting setting picks your
  servers or Cloudflare (Workers static assets). Modula builds the site,
  uploads it, and keeps the Worker's variables, secrets and custom domains in
  sync.
- **Environments:** a switcher that remembers your choice; new environments as
  a duplicate (sealed variables stay sealed, fresh database credentials, a
  branch per repo service) or empty; rename; the default environment can't be
  deleted. CLI: `modula environment new --duplicate --branch`, `rename`,
  `delete`.
- **Team members:** invite people to a workspace with a single-use link and a
  role; change roles, remove members, or leave. An admin invite needs sudo.
- **Cron schedules:** run a service on a 5-field UTC schedule instead of
  continuously, with run history and "Run now" (`modula cron run`).
- **PR previews:** an environment per pull request, built from a base
  environment, with a generated domain and a PR comment linking it; removed
  when the PR closes. Fork PRs get no secrets.
- **Wait for CI:** a push deploys once its GitHub checks pass.
- **Shell:** a terminal into a running deployment in the dashboard
  (`modula ssh`), and `modula run <cmd>` to run a command locally with a
  service's variables.
- **Alerts:** Slack, Discord and webhook channels, with rules for failed,
  crashed or degraded deployments, CPU, memory and disk use, and offline
  servers.
- **Edge:** HTTP access logs and request, error-rate and latency charts per
  service; edge rules to block, allow, redirect, rate-limit, cache or set
  headers.
- **Templates:** one-click apps (n8n, Plausible, Umami, Metabase, Uptime Kuma,
  Ghost), with images pinned by digest (`modula deploy --template`).
- **Postgres:** back up to an S3 target, an R2 or self-hosted bucket, or a
  server's disk; point-in-time recovery with WAL-G; read replicas with
  replication lag and promote.
- **Serverless services:** scale a service to zero after an idle time; the
  next request starts it again (cold starts are shown on the deployment).
- **Functions:** JavaScript functions written in the dashboard or bundled from
  a repository, run by workerd on your servers or deployed to Cloudflare
  Workers.
- **Private network:** DNS records for every server running a replica, a
  virtual IP per service balanced across them, relay through the main server
  for servers behind NAT, and a Hetzner firewall rule for WireGuard. A
  deployment whose replica crashed shows as degraded.

### Fixed

- New canvas cards no longer land on top of existing ones.

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

[Unreleased]: https://github.com/Modula-Labs/modula/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/Modula-Labs/modula/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Modula-Labs/modula/releases/tag/v0.1.0
