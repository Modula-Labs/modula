# Modula Deploy

Self-hosted platform for deploying apps and databases on your own servers.
Push to GitHub or pick an image, and Modula builds, deploys, routes and secures
it. The workflow is modeled on Railway's: a project canvas, environments, staged
changes, variables with references, domains, database backups and a CLI.

> **Status: alpha.** It runs real apps, but expect rough edges and breaking
> changes before 1.0. Today an instance has a single owner account; teams and
> invites come later. Read [SECURITY.md](SECURITY.md) before exposing it.

## Features

- **Deploy** from GitHub (auto-deploy on push, watch paths), a Docker image, or
  `modula up` from a local directory. Builds use [Railpack](https://railpack.com)
  or your Dockerfile, in a sandboxed BuildKit per workspace.
- **Project canvas** with environments, pending changes you review and deploy
  together, health checks, pre-deploy commands, draining, rollback (rebuilds the
  old commit) and live logs and metrics.
- **Variables**: per-service and shared, references (`${{ Postgres.DATABASE_URL }}`),
  sealed values, encrypted at rest with keys per environment.
- **Databases**: Postgres, MySQL, Redis, MongoDB templates with volumes, private
  networking, TCP proxies, a Database tab (tables, rows, SQL, stats) and
  encrypted backups to S3, R2, B2 or MinIO.
- **Domains**: generated domains and custom domains with automatic TLS (Caddy),
  optional Cloudflare DNS.
- **Servers**: add more servers with one command or create them on Hetzner from
  the dashboard; services are placed by load and location.
- **Security**: 2FA (TOTP), sudo mode for destructive actions, scoped API tokens,
  per-route authorization, an audit log. See [guide/security.md](guide/security.md).
- **CLI and config as code**: `modula` mirrors the Railway CLI's commands;
  `modula.toml` keeps a whole project in git.

## Install

On a fresh Ubuntu 22.04+ or Debian 12+ server (2 GB RAM or more):

```bash
curl -fsSL https://usemodula.org/install.sh | bash
```

No domain is needed to start: the dashboard comes up on `https://<server IP>`
and you can set a domain later in **Settings → Domain**. The installer sets up
Postgres, Redis, Docker and Caddy, downloads binaries verified against a signed
checksum list, writes hardened systemd units, and prints a one-time setup token.
It also writes `/root/modula-recovery-kit.txt` with the master key: copy it
somewhere safe and delete it from the server.

Full details, including unattended installs, ports, upgrades and recovery:
[guide/install.md](guide/install.md).

## Documentation

The [guide](guide/README.md) covers everything: projects and services,
variables, deployments, databases, backups, domains, servers, the CLI, the API,
and [migrating from Railway](guide/migrating-from-railway.md).

## Layout

```
crates/server   modula-server: axum API, auth, authz, jobs, migrations, embedded UI,
                internal CA, node endpoint (gRPC), deployments
crates/node     modula-node: runs on every server; Docker runtime, Caddy, builds, databases
crates/cli      the `modula` CLI
crates/crypto   envelope encryption (XChaCha20-Poly1305, AAD-bound), tokens, argon2id
crates/proto    modula.node.v1 gRPC protocol between server and node
crates/backup   backup streaming, age encryption, outbound network guard
crates/hetzner  Hetzner Cloud API client
web/            React SPA (served by modula-server)
deploy/         install.sh, release public key, e2e installer test
scripts/        check.sh (what CI runs), release.sh, dev-*.sh
guide/          user documentation
```

## Develop

Prerequisites and conventions are in [CONTRIBUTING.md](CONTRIBUTING.md).

```bash
scripts/dev-services.sh                      # Postgres :55432, Redis :56379 (127.0.0.1 only)
cp .env.example .env && set -a && . ./.env && set +a
(cd web && pnpm install && pnpm build)       # without web/dist the UI is blank
cargo run -p modula-server -- keygen --out ./dev-master.key
cargo run -p modula-server                   # prints the one-time setup token
scripts/check.sh                             # fmt, clippy, tests, deny, audit, no-shell
```

Run a server (node) against it. The join command comes from **Servers → Add
server** or `POST /api/nodes/join-tokens`:

```bash
MODULA_JOIN_TOKEN=mcj_… cargo run -p modula-node -- --data-dir .dev/node join --server https://localhost:7101 --ca sha256:…
cargo run -p modula-node -- --data-dir .dev/node run
cargo test -p modula-node --test docker -- --ignored    # real Docker round trip
```

On macOS, Docker Desktop can't route to container IPs from the host, so health
checks only pass when `modula-node` runs on Linux. `scripts/dev-node.sh` starts a
simulated server (a privileged container with its own dockerd, Caddy and node)
joined to `scripts/dev-server.sh`:

```bash
scripts/dev-node.sh dev-node-1 8080      # apps on http://<generated>.localhost:8080
scripts/dev-node.sh dev-node-1 --rm      # remove it
```

For the UI with hot reload: `scripts/dev-server.sh`, then `cd web && pnpm dev`
(start the server with `MODULA_PUBLIC_URL=http://localhost:5173`).

Releases are built and signed with `scripts/release.sh`; the signing key lives
outside the repository and its public half is `deploy/release.pub.pem`.
`deploy/e2e/run.sh` runs the installer end to end in systemd containers.

## Security

Please report vulnerabilities privately, as described in [SECURITY.md](SECURITY.md).
The design in brief:

- Every route declares its access policy; resource routes resolve the resource's
  workspace and check your role before the handler runs. Non-members get 404.
- Mutations require an exact `Origin` match (CSRF); sessions are hashed,
  `__Host-` HttpOnly cookies, rotated on login and sudo.
- Secrets are sealed with keys per environment and AAD bound to their row;
  revealing a variable needs sudo and is audited.
- Servers enroll with a CSR and a single-use join token over mTLS pinned to the
  instance's own CA; revoked servers can't reconnect.
- Containers drop capabilities, run with `no-new-privileges`, default memory and
  CPU limits, one network per environment, and no access to cloud metadata.
  Builds run in a sandboxed BuildKit per workspace.
- No shell strings anywhere (`scripts/no-shell.sh` enforces it in CI).

## License

[AGPL-3.0-only](LICENSE). Third-party components and their licenses are listed in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

Modula Deploy is not affiliated with or endorsed by Railway Corp. "Railway" is
their trademark and is used here only to describe compatibility.
