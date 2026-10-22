# Modula Cloud

Self-hostable PaaS with a Railway-style workflow: project canvas, environments,
staged changes, GitHub auto-deploys, variables, domains, database backups, and
the `modula` CLI. Rust control plane + a Rust agent per node; Docker runtime,
Caddy edge, Railpack builds.

Status: **C0b — auth** (C0a foundations plus TOTP 2FA, recovery codes, sudo
mode, sessions, and the web shell: setup, sign-in, 2FA, projects, account security).

## Layout

```
crates/crypto   envelope encryption (XChaCha20-Poly1305, AAD-bound), tokens, argon2id
crates/server   modula-server: axum API, sessions, 2FA, authz, audit log, jobs, migrations, embedded UI
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

## Security model (enforced in code and tests)

- **Routes declare access.** Every route is registered through `ApiRouter` with
  `Public`, `Authenticated`, or `Resource(Action)`. Resource routes carry exactly
  one `{x_id}` param; the router resolves it to a workspace and checks the
  caller's role *before* the handler or body parsing runs. Non-members get 404.
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
- **Secrets:** a master key file (0600) wraps per-workspace data keys; fields are
  sealed with AAD bound to (workspace, table, column, row).
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
