# Modula Deploy

Self-hostable PaaS with a Railway-style workflow: project canvas, environments,
staged changes, GitHub auto-deploys, variables, domains, database backups, and
the `modula` CLI. Rust control plane + `modula-node` on each server; Docker runtime,
Caddy edge, Railpack builds.

Status: **C1b — edge and canvas (in progress).** On top of C0 (auth, 2FA, sudo
mode) and C1a (servers join over mTLS; projects, environments, services,
encrypted variables and image deployments): every server runs Caddy, configured
by `modula-node`, with generated domains and on-demand TLS; the dashboard has the
project canvas, service and deployment sheets, live logs, variables and a
Servers page. Next in C1b: `install.sh` (install, upgrade, recovery kit).

## Install

On a fresh Ubuntu 22.04+/Debian 12+ server with DNS for the dashboard (and,
optionally, a wildcard for generated app domains) pointing at it:

```bash
curl -fsSL https://usemodula.org/install.sh \
  | sudo bash -s -- --domain deploy.example.com --email you@example.com --apps-domain apps.example.com
```

It installs Postgres, Redis and Docker from the distribution (Docker only if
missing), downloads prebuilt binaries checked against Ed25519-signed SHA-256
sums, writes hardened systemd units, joins the server to itself, and prints the
one-time setup token. It also writes `/root/modula-recovery-kit.txt` with the
master key: copy it somewhere safe and delete it from the server.

More servers join from **Servers → Add server**, which prints a one-line
command that fetches the installer from your instance. Upgrade with
`… install.sh | sudo bash -s -- upgrade`; it swaps the release atomically and
rolls back if the new version doesn't come up.

Releases are built with `scripts/release.sh` and signed with
`.dev/release-signing/release.key` (keep it offline; its public half is
`deploy/release.pub.pem`, embedded in `install.sh`). `deploy/e2e/run.sh` runs
the installer end to end in systemd containers.

## Layout

```
crates/crypto   envelope encryption (XChaCha20-Poly1305, AAD-bound), tokens, argon2id
crates/proto    modula.node.v1 gRPC protocol between the server and modula-node
crates/server   modula-server: axum API, sessions, 2FA, authz, audit log, jobs, migrations,
                embedded UI, internal CA, node endpoint (gRPC :7101), deployments
crates/node     modula-node: join, one outbound mTLS stream, reconciler, Docker runtime
web/            React SPA (served by modula-server)
deploy/         install.sh, release.pub.pem, e2e/ (install test), dev/ (simulated server)
scripts/        check.sh (everything CI runs), release.sh, dev-*.sh, no-shell.sh
```

## Develop

```bash
scripts/dev-services.sh                      # Postgres :55432, Redis :56379 (127.0.0.1 only)
cp .env.example .env && set -a && . ./.env && set +a
cargo run -p modula-server -- keygen --out ./dev-master.key
cargo run -p modula-server                   # prints the one-time setup token
scripts/check.sh                             # fmt, clippy, tests, deny, audit, no-shell
```

Run a server (node) against it. The join command comes from
`POST /api/nodes/join-tokens` (instance admins, sudo):

```bash
cargo run -p modula-node -- --data-dir .dev/node join --server https://localhost:7101 --token mcj_… --ca sha256:…
cargo run -p modula-node -- --data-dir .dev/node run
cargo test -p modula-node --test docker -- --ignored    # real Docker round trip
```

On macOS, Docker Desktop can't route to container IPs from the host, so HTTP
health checks only pass when `modula-node` itself runs on Linux. For that, run a
simulated server: a privileged container with its own dockerd, Caddy and
`modula-node`, joined to the dev server (`scripts/dev-server.sh` must be running):

```bash
scripts/dev-node.sh dev-node-1 8080      # apps on http://<generated>.localhost:8080
scripts/dev-node.sh dev-node-1 --rm      # remove it
```

## Security model (enforced in code and tests)

- **Routes declare access.** Every route is registered through `ApiRouter` with
  `Public`, `Authenticated`, `Sudo`, `Instance(Action)` (instance admins) or
  `Resource(Action)`, where each action names the one resource kind it applies
  to. Resource routes carry exactly one `{x_id}` param; the router resolves it
  to its owner and checks the caller's role *before* the handler or body
  parsing runs. Non-members get 404.
  `tests/policy.rs` checks the public allowlist, that every guarded route rejects
  anonymous callers, and that every resource route hides other workspaces.
- **CSRF:** mutations require `Origin` to equal `MODULA_PUBLIC_URL` exactly.
- **Sessions:** 256-bit opaque tokens, stored as SHA-256 hashes, `__Host-`
  HttpOnly/Secure/SameSite=Lax cookies, rotated on login, idle + absolute expiry.
- **First admin:** only via a one-time setup token printed by the server.
- **2FA:** TOTP (RFC 6238, ±1 step, replay-proof via a stored last step) and
  single-use recovery codes (HMAC under a server pepper). Login with 2FA issues a
  separate short-lived pending session; 5 wrong codes burn it.
- **Sudo mode:** deleting, disabling 2FA, regenerating codes and changing the
  password need a re-authentication within 15 minutes (second factor if enrolled);
  the session token rotates.
- **Secrets:** a master key file (0600) wraps data keys per scope: one per
  environment for variables and deployment snapshots, so environments never
  share a key. Fields are sealed with AAD bound to (scope, table, column, row).
  Variable values are write-only over the API; revealing one needs sudo and is
  audited; sealed variables can never be revealed. Deploy logs mask values.
- **Servers:** nodes enroll with a CSR (the key never leaves the server) and a
  single-use, hashed, 1-hour join token, and pin the root CA fingerprint. The
  node endpoint accepts only the node's current certificate serial, so revoking
  a server cuts it off. A node can only report on deployments placed on it.
- **Edge:** Caddy's config is replaced as a whole by `modula-node` over a unix
  admin socket (`persist: false`). Hostnames route only to a serving deployment;
  on-demand TLS asks the node, which allows only hostnames it routes.
- **Containers:** all capabilities dropped except a small allowlist,
  `no-new-privileges`, an init process, pids/memory/CPU limits, one network per
  environment, nothing published on the host. Config fields like `privileged`
  don't exist in the API.
- **No shell strings:** `scripts/no-shell.sh` fails CI on shell invocations.
- **Instance admins** get no implicit access to workspaces.

## Web UI

`web/` is a Vite + React SPA (TanStack Router/Query, Tailwind v4 on the tokens in
`docs/ui-spec.md` §8). `pnpm build` writes `web/dist`, which is embedded into
`modula-server` at compile time and served with the same strict CSP.

```bash
scripts/dev-server.sh            # embedded UI on http://localhost:7100
cd web && pnpm dev               # hot reload on :5173 (start the server with MODULA_PUBLIC_URL=http://localhost:5173)
```
