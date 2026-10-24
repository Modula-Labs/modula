# Contributing to Modula Deploy

Thanks for helping. Bug reports, fixes, docs and features are all welcome. For
anything bigger than a small fix, please open an issue first so we can agree on the
approach before you write a lot of code.

Security problems are the exception: never file them as issues. See
[SECURITY.md](SECURITY.md).

By participating you agree to follow our [Code of Conduct](CODE_OF_CONDUCT.md).

## Prerequisites

- **Rust 1.95** — pinned in [`rust-toolchain.toml`](rust-toolchain.toml); with
  [rustup](https://rustup.rs) installed, `cargo` picks it (plus rustfmt and clippy)
  automatically.
- **protoc** (the Protocol Buffers compiler) — `modula-proto` generates the node
  gRPC code at build time and fails without it.
  macOS: `brew install protobuf` · Debian/Ubuntu: `sudo apt-get install protobuf-compiler`.
- **Node.js 22 and pnpm 8** for the web UI in `web/` (the lockfile is pnpm 8's
  format; newer pnpm majors will want to rewrite it).
- **Docker** — `scripts/dev-services.sh` runs Postgres and Redis in containers, and
  `scripts/dev-node.sh` runs simulated servers.
- Optional: [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) and
  [`cargo-audit`](https://github.com/rustsec/rustsec) (`scripts/check.sh` skips them
  when missing; CI always runs them).

## Running it locally

```bash
scripts/dev-services.sh      # Postgres on 127.0.0.1:55432, Redis on 127.0.0.1:56379
scripts/dev-server.sh        # builds web/ if needed, creates .dev/master.key, runs modula-server
```

`scripts/dev-server.sh` serves the dashboard with the embedded UI on
<http://localhost:7100> and prints the one-time setup token for the first admin on
first start. `--rebuild-web` forces a fresh `pnpm build`. The settings it uses are
in `scripts/dev-env.sh`; local state lives in `.dev/` (git-ignored).

To run apps you need a server (node). On macOS, Docker Desktop can't route to
container IPs from the host, so use a simulated server: a privileged container with
its own dockerd, Caddy and `modula-node`, joined to the dev control plane:

```bash
scripts/dev-node.sh dev-node-1 8080   # apps on http://<generated>.localhost:8080
scripts/dev-node.sh --rm dev-node-1   # remove it again
```

The README's **Develop** section also shows how to run `modula-server` and
`modula-node` directly with `cargo run`.

### The web UI

The dashboard is a Vite + React app in `web/`. `pnpm build` writes `web/dist`, which
`modula-server` **embeds at compile time**. If `web/dist` doesn't exist, the server
still builds and runs, but serves a blank UI. For UI work, run the Vite dev server
(port 5173, proxying `/api` to `127.0.0.1:7100`) and start `modula-server` with
`MODULA_PUBLIC_URL=http://localhost:5173` so the browser's `Origin` passes the CSRF
check:

```bash
MODULA_PUBLIC_URL=http://localhost:5173 scripts/dev-server.sh
cd web && pnpm install --frozen-lockfile && pnpm dev
```

and rebuild (`pnpm build`, or `scripts/dev-server.sh --rebuild-web`) before testing
the embedded UI.

## Before you open a pull request

Run everything CI runs:

```bash
scripts/check.sh
```

It needs the dev services (`DATABASE_URL` defaults to the one
`scripts/dev-services.sh` starts) and runs `scripts/no-shell.sh`, the web
typecheck, tests and build, `cargo fmt --check`, `cargo clippy -D warnings`,
`cargo test --workspace`, and `cargo deny` / `cargo audit` when installed.

Please also:

- add or update tests for what you change: behaviour tests over the HTTP API and
  real Postgres are preferred to mocks; skip tests that only restate the code;
- update the user docs in `guide/` when behaviour visible to users changes;
- keep pull requests focused on one change and describe the why, not only the what.

## Conventions

- **No shell strings.** Never spawn `sh -c` or build a command line from strings;
  pass arguments as separate argv items (`Command::new(bin).args([...])`).
  `scripts/no-shell.sh` enforces this in CI. A reviewed line that runs a fixed
  script with no user input can opt out with a `no-shell: allow (<reason>)` comment.
- **SQL with sqlx, no ORM.** Write queries by hand with `sqlx`.
- **One migration file during pre-release.** Schema changes go into
  `crates/server/migrations/0001_init.sql` rather than new migration files, until
  the first stable release.
- **Short ids.** Resources use the short ids from `modula-id`, not UUIDs.
- **Routes declare access.** Every API route is registered through `ApiRouter` with
  its access level; `tests/policy.rs` checks them (see the README's security model).
- **Dependencies:** new crates must pass `cargo deny` (licenses in `deny.toml`);
  pin npm dependencies to exact versions.

## License of contributions

Modula Deploy is licensed under the [GNU AGPL v3.0 only](LICENSE). By submitting a
contribution, you agree that it is licensed under **AGPL-3.0-only**, and you confirm
that you have the right to submit it under that license. We don't require a CLA or
`Signed-off-by` lines.
