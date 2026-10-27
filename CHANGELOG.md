# Changelog

All notable changes to Modula Deploy are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) (pre-1.0: minor versions
may break things).

## [Unreleased]

## [0.1.10] - 2026-10-27

### Fixed

- **Deleting a service from one environment no longer deletes it
  everywhere.** Deleting a service in the dashboard while viewing `staging`
  removed it from every environment, production's deployments and volume
  data included. Deleting is now per environment, like Railway: the confirm
  dialog says **Delete <name> from <environment>**, and only that
  environment's deployments, volume data, domains, TCP proxy, variables and
  cron schedule go. The service itself goes with its last environment.
  Deleting from all environments is a separate option in the dialog that
  asks for the service's name. The settings-tab Delete buttons (services,
  functions, buckets) now confirm first, and the canvas and the project's
  Danger settings only list the services of the environment being viewed.
- Discarding a never-deployed service's pending changes removes it from that
  environment only.

### Changed

- **API (breaking):** `DELETE /api/services/{id}` now requires
  `?allEnvironments=true` and is refused without it, so an old client can't
  delete a service everywhere by mistake. The new `DELETE
  /api/service-instances/{id}` deletes a service from one environment
  (Member + sudo); its response says whether the service itself was deleted
  (`serviceDeleted`).
- The `service.delete` audit entry records the environment
  (`environment`, `environmentName`, `serviceDeleted`), or
  `allEnvironments: true` and the environments for a delete everywhere.

## [0.1.9] - 2026-10-26

### Fixed

- Retrying a server migration within half an hour no longer fails with
  "a server with that name exists".
- `install.sh main --restore` rejects migration ids the control plane can't
  read, instead of leaving it crash-looping after the restore.

## [0.1.8] - 2026-10-26

**Upgrading from 0.1.7:** the database needs a manual schema patch before
the upgrade (services lose `display_name`; new tables for server metrics,
migrations and service moves; new columns on provisioned servers).

### Added

- **Migrate the main server.** Servers → main server → Migrate moves the
  control plane too: services move to a new server, the dashboard goes
  read-only for a few minutes, the new server installs a copy of the control
  plane (`install.sh main --restore`, fetched over its node connection with a
  single-use ticket), then every server switches to it (`ServerAddress`,
  probed before switching), Modula-managed Cloudflare DNS is repointed, and
  the old dashboard redirects. `GET /api/maintenance` reports the state; the
  dashboard shows "Moving to the new server…" and follows. Refused while
  private mode is on.

- **Placement prefers workers near what a service uses.** A new service's
  first deployment picks an online server that isn't the main one (when
  another is usable), closest to the services it references through
  variables (else to where most of its environment runs), then the least
  loaded. The server picker lists that recommended server first and labels
  the main server; `GET /api/environments/{id}/placement-suggestion` returns
  the ranking.
- **Project warnings.** A "N warnings" button on the project page (hidden
  when there are none) opens a panel of the environment's problems: failed or
  crashed deployments, services on an offline server or on the main server,
  nearly full disks, and references to missing services or shared variables.
  Each links to the docs ("Why?") and opens its fix: the deployment's logs,
  or the service's server picker with the recommended server selected.
  `GET /api/environments/{id}/warnings`.

### Changed

- **Services have one name.** The separate display name is gone: the name is
  what the dashboard shows, the private hostname and the reference prefix.
  Renaming a service (Settings → Service, `modula rename`, or
  `PATCH /api/services/{id}` with `{ "name" }`) rewrites every
  `${{old.VAR}}` reference in the project's service and shared variables,
  across environments, and marks the affected services pending. The old
  private hostname keeps resolving for deployments from before the rename.
  Name inputs turn what you type into a valid name as you go.

- `modula variables` lists values, like `railway variables`: references
  expanded, `--kv` prints `NAME=value` lines and `--json` a `{NAME: value}`
  object. Sealed values stay hidden; each listing is audited.
- `modula tunnel [SERVICE]` (= `connect --tunnel-only`) prints the connection
  string with its password by default; `--hide-password` masks it.
- **Builders default to Automatic**, as on Railway: the Dockerfile when the
  root directory has one (or `dockerfilePath` exists), else Railpack.
  Existing services keep their builder. A Dockerfile image runs its own
  `CMD` unless you set a start command; start commands detected for
  Railpack (monorepo apps) are not used for it.
- Build logs show the builder and start command they resolved and where
  each came from.

### Fixed

- A Railpack build that fails while the build root has a Dockerfile says so
  and suggests the Dockerfile builder; a container exiting with 127 at
  startup names the start command it ran and where that came from.

## [0.1.7] - 2026-10-26

**Upgrading from 0.1.6:** the database needs one manual change before the
upgrade (the passkey tables in the edited initial migration); see the
release notes.

### Added

- **Passkeys as a second factor**: add them under Account → Security and
  confirm sign-ins and sudo with your device or security key, alone or next
  to an authenticator app. Recovery codes now come with the first second
  factor of either kind, and every 2FA prompt offers "Lost your 2FA device?
  Use a recovery code". Passkeys need a dashboard domain.

### Changed

- **Confirming it's you lasts a day.** Sudo mode (destructive and
  secret-revealing actions) stays unlocked for 24 hours after a sign-in or
  re-authentication, instead of 15 minutes. API tokens still never get it.

### Changed

- **Adding services works like Railway.** An empty project shows the add
  picker right on the canvas, and **Add** opens it without dimming the app.
  The GitHub step asks only for the account and repository: picking one adds
  its services at once (one per monorepo app, named after its package) as
  **New** cards waiting in **Apply changes**; set the branch, port,
  variables and name on each before deploying.
- Uptime bars are square and thinner.

## [0.1.6] - 2026-10-26

**Upgrading from 0.1.5:** the database needs one manual change before the
upgrade (a new `services.display_name` column in the edited initial
migration); see the release notes.

### Added

- **New services wait as drafts, like Railway.** Adding a service, database,
  bucket, function or template app no longer deploys it: it shows on the
  canvas as a green **New** card and in **Apply changes** as "will be added",
  with its settings, variables and domains, so you can finish its settings
  (domain, variables) first. **Deploy** (Shift+Enter) creates it with the
  rest; **Discard** removes a service that was never deployed.
- **Monorepo detection.** Picking a GitHub repository with a pnpm, npm, yarn
  or bun workspace offers one service per app (a package with a `start` or
  `dev` script), built from the repository root with the package manager's
  filter (`pnpm --filter @acme/api build`) and deployed only when its own
  directory changes.
- **Display names.** A service can show a name like `@acme/api` (detected
  services get their package name); its name stays the private hostname and
  the prefix of `${{name.VAR}}` references. Edit it under Settings → Service.

### Changed

- The service panel and its settings no longer have their own **Deploy**
  button: deploys go through **Apply changes**.
- **Connecting GitHub needs no typing.** The GitHub App Modula creates is now
  public, so one App installs on your personal account and any organization:
  GitHub's installation page lists them to pick from, and **Install on
  another account** adds more later. The "Personal account / Organization"
  choice and the organization name field are gone (an organization can still
  own the App, under **Advanced**). Installations still bind to a workspace
  only through this instance's own flow (single-use `state` plus proof that
  the GitHub user can access the installation); webhooks from installations
  no workspace bound are dropped. Existing private Apps keep working.

### Fixed

- **Build logs stayed empty** ("no build log on this server") until the tab
  was reopened: a followed build log now waits for the build's first line.
- **Every Railpack build failed** with `mkdir /root/.docker: read-only file
  system`: buildctl now keeps its config in the node's data directory.

## [0.1.5] - 2026-10-26

### Fixed

- **Fresh installs stopped at "Starting the node and edge"**: the node never
  started (a missing socket unit ended the step). Fixed in `install.sh`.
- `modula server create`: a network error talking to Cloudflare keeps the
  token and offers to retry (and says why); the DNS question isn't asked
  again after picking a Cloudflare domain; an empty subdomain means the
  domain itself; the summary names the record it creates.

## [0.1.4] - 2026-10-25

### Added

- **`modula update`** updates the CLI to the newest release (signature and
  checksum verified like `install-cli.sh`, then the executable is replaced in
  place); `--check` only reports.
- **`modula server update`** shows the logged-in instance's version and the
  available one, starts the upgrade (API tokens no longer need a recent
  sign-in for it) and follows it until the control plane and every server
  are done; `--check` only reports.
- **Update offers:** at a terminal, after a command, the CLI offers a newer
  CLI or server version: update now, remind me tomorrow, or skip this
  version. Checked at most once a day in the background (never slows a
  command); off in CI, with `--json`, or with `MODULA_NO_UPDATE_CHECK=1`.
- **Dashboard update banner** for instance admins: update now, later, or skip
  this version.
- **`modula server create` connects Cloudflare:** pick one of your domains
  and a subdomain; the DNS record is created for you.

### Changed

- `modula server create` shows each server's disk instead of its traffic.

## [0.1.3] - 2026-10-25

### Added

- **Network logs** on a deployment: its HTTP requests (time, method, path,
  status, duration), searchable with `@path:`, `@httpStatus:` and `@method:`.
- **Deploy and build logs** as a time and data table with error and warning
  lines marked, a detail view per line (attributes, raw data, JSON fields,
  view in context) and Load more for older output.
- **Uptime and 5xx strips** on service and project metrics: up, down
  (crashed or unreachable) or no data per bar, and a green-to-red bar by the
  share of 5xx responses.

### Fixed

- Stored log lines (the Logs page) now mask secret values like the live stream.

### Changed

- `modula server create`: each location shows what its cheapest server gets
  you (vCPU, RAM, disk, architecture, included traffic), cheapest first,
  with a note on why prices differ; server sizes show included traffic too.

## [0.1.2] - 2026-10-25

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

### Fixed

- `install.sh` stops with a clear message off Linux instead of failing on
  macOS's bash 3.2.
- Prompts started from `install-cli.sh`'s menu read the keyboard on macOS.

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

[Unreleased]: https://github.com/Modula-Labs/modula/compare/v0.1.10...HEAD
[0.1.10]: https://github.com/Modula-Labs/modula/compare/v0.1.9...v0.1.10
[0.1.9]: https://github.com/Modula-Labs/modula/compare/v0.1.8...v0.1.9
[0.1.8]: https://github.com/Modula-Labs/modula/compare/v0.1.7...v0.1.8
[0.1.5]: https://github.com/Modula-Labs/modula/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/Modula-Labs/modula/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/Modula-Labs/modula/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/Modula-Labs/modula/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/Modula-Labs/modula/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Modula-Labs/modula/releases/tag/v0.1.0
