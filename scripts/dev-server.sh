#!/usr/bin/env bash
# Run modula-server against the local dev services with the embedded web UI.
# Usage: scripts/dev-server.sh [--rebuild-web]
set -euo pipefail
cd "$(dirname "$0")/.."

scripts/dev-services.sh >/dev/null
if [[ "${1:-}" == "--rebuild-web" || ! -f web/dist/index.html ]]; then
  (cd web && pnpm install --frozen-lockfile >/dev/null && pnpm build >/dev/null)
fi
docker exec modula-cloud-pg psql -U modula -tc "SELECT 1 FROM pg_database WHERE datname='modula_dev'" | grep -q 1 \
  || docker exec modula-cloud-pg psql -U modula -c "CREATE DATABASE modula_dev" >/dev/null

mkdir -p .dev
[[ -f .dev/master.key ]] || cargo run -q -p modula-server -- keygen --out .dev/master.key

export MODULA_DATABASE_URL=postgres://modula:modula@127.0.0.1:55432/modula_dev
export MODULA_REDIS_URL=redis://:modula@127.0.0.1:56379
export MODULA_PUBLIC_URL=${MODULA_PUBLIC_URL:-http://localhost:7100}
export MODULA_LISTEN=${MODULA_LISTEN:-127.0.0.1:7100}
export MODULA_MASTER_KEY_FILE=.dev/master.key
exec cargo run -q -p modula-server -- serve
