#!/usr/bin/env bash
# Start a simulated server joined to the local dev control plane
# (scripts/dev-server.sh must be running).
#
#   scripts/dev-node.sh [name] [http-port]     default: dev-node-1 8080
#   scripts/dev-node.sh --rm [name]            remove it (and its data)
#
# Apps routed by it are reachable at http://<generated-host>.localhost:<http-port>.
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "${1:-}" == "--rm" ]]; then
  name=${2:-dev-node-1}
  docker rm -f "$name" >/dev/null 2>&1 || true
  docker volume rm "$name-data" "$name-docker" >/dev/null 2>&1 || true
  echo "$name: removed"
  exit 0
fi

name=${1:-dev-node-1}
port=${2:-8080}
build=.dev/node-image
mkdir -p "$build"

# Compile modula-node for Linux inside a container (cached across runs).
docker run --rm \
  -v "$PWD":/src:ro \
  -v modula-cargo-registry:/usr/local/cargo/registry \
  -v modula-node-target:/target \
  -e CARGO_TARGET_DIR=/target \
  -w /src rust:1-alpine \
  sh -c 'apk add -q --no-cache musl-dev protobuf-dev >/dev/null && cargo build -q --release -p modula-node && cp /target/release/modula-node /target/modula-node.out'
docker run --rm -v modula-node-target:/target -v "$PWD/$build":/out alpine cp /target/modula-node.out /out/modula-node
cp deploy/dev/node.Dockerfile deploy/dev/entrypoint.sh "$build/"
docker build -q -t modula-dev-node -f "$build/node.Dockerfile" "$build" >/dev/null

if docker ps -a --format '{{.Names}}' | grep -qx "$name"; then
  docker start "$name" >/dev/null
  echo "$name: started (already joined)"
  exit 0
fi

# A fresh single-use join command from the dev control plane.
set -a; . scripts/dev-env.sh; set +a
join=$(cargo run -q -p modula-server -- join-token --ttl-minutes 10 2>/dev/null)
server=$(sed -E 's/.*--server ([^ ]+).*/\1/' <<<"$join")
token=$(sed -E 's/.*--token ([^ ]+).*/\1/' <<<"$join")
ca=$(sed -E 's/.*--ca ([^ ]+).*/\1/' <<<"$join")

docker run -d --privileged --name "$name" \
  -p "127.0.0.1:$port:80" \
  -e MODULA_SERVER="$server" -e MODULA_JOIN_TOKEN="$token" -e MODULA_CA="$ca" -e MODULA_NODE_NAME="$name" \
  -v "$name-data:/var/lib/modula-node" -v "$name-docker:/var/lib/docker" \
  modula-dev-node >/dev/null
echo "$name: running; apps on http://<host>.localhost:$port"
