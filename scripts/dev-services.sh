#!/usr/bin/env bash
# Local Postgres + Redis for development and tests, bound to 127.0.0.1 only.
set -euo pipefail

start() {
  local name=$1; shift
  if docker ps -a --format '{{.Names}}' | grep -qx "$name"; then
    docker start "$name" >/dev/null
  else
    docker run -d --name "$name" "$@" >/dev/null
  fi
  echo "$name: up"
}

start modula-cloud-pg -e POSTGRES_USER=modula -e POSTGRES_PASSWORD=modula -e POSTGRES_DB=modula \
  -p 127.0.0.1:55432:5432 postgres:17-alpine
start modula-cloud-redis -p 127.0.0.1:56379:6379 redis:7-alpine redis-server --requirepass modula
