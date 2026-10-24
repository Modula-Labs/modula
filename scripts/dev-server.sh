#!/usr/bin/env bash
# Run modula-server against the local dev services with the embedded web UI.
# Usage: scripts/dev-server.sh [--rebuild-web]
set -euo pipefail
cd "$(dirname "$0")/.."

scripts/dev-services.sh >/dev/null
if [[ "${1:-}" == "--rebuild-web" || ! -f web/dist/index.html ]]; then
  (cd web && pnpm install --frozen-lockfile >/dev/null && pnpm build >/dev/null)
fi
db=${MODULA_DEV_DB:-modula_dev}
docker exec modula-cloud-pg psql -U modula -tc "SELECT 1 FROM pg_database WHERE datname='$db'" | grep -q 1 \
  || docker exec modula-cloud-pg psql -U modula -c "CREATE DATABASE $db" >/dev/null

mkdir -p .dev
[[ -f .dev/master.key ]] || cargo run -q -p modula-server -- keygen --out .dev/master.key

set -a; . scripts/dev-env.sh; set +a
exec cargo run -q -p modula-server -- serve
