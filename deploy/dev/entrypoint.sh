#!/bin/sh
# Boot a simulated server: Docker engine, Caddy (admin on a unix socket),
# then join the control plane once and run modula-node.
set -eu

dockerd-entrypoint.sh dockerd >/var/log/dockerd.log 2>&1 &
i=0
until docker info >/dev/null 2>&1; do
  i=$((i + 1))
  [ "$i" -lt 120 ] || { echo "dockerd did not start" >&2; tail -20 /var/log/dockerd.log >&2; exit 1; }
  sleep 0.5
done

mkdir -p /run/caddy
printf '{"admin":{"listen":"unix//run/caddy/admin.sock"}}' >/etc/caddy.json
caddy run --config /etc/caddy.json >/var/log/caddy.log 2>&1 &

if [ ! -f /var/lib/modula-node/node.json ]; then
  modula-node join --server "$MODULA_SERVER" --token "$MODULA_JOIN_TOKEN" --ca "$MODULA_CA" --name "${MODULA_NODE_NAME:-dev-node}"
fi
exec modula-node run --caddy-admin unix:/run/caddy/admin.sock
