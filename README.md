# Modula Deploy

Self-hostable PaaS with a Railway-style workflow: project canvas, environments,
staged changes, GitHub auto-deploys, variables, domains, database backups, and
the `modula-deploy` CLI. Rust control plane + `modula-node` on each server; Docker runtime,
Caddy edge, Railpack builds.

Status: **C1a — servers and image deploys.** On top of C0 (auth, 2FA, sudo
mode, web shell): servers join over mTLS with single-use tokens, and projects,
environments, services, encrypted variables and image deployments work through
the API, with live deploy logs. No UI for these yet (C1b).

## Layout

```
crates/crypto   envelope encryption (XChaCha20-Poly1305, AAD-bound), tokens, argon2id
crates/proto    modula.node.v1 gRPC protocol between the server and modula-node
crates/server   modula-server: axum API, sessions, 2FA, authz, audit log, jobs, migrations,
                embedded UI, internal CA, node endpoint (gRPC :7101), deployments
crates/node     modula-node: join, one outbound mTLS stream, reconciler, Docker runtime
web/            React SPA (served by modula-server)
scripts/        check.sh (everything CI runs), dev-services.sh, no-shell.sh
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
health checks only pass when `modula-node` itself runs on Linux.

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
