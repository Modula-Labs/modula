#!/bin/sh
# Boot a simulated server: Docker engine, Caddy (admin on a unix socket),
# then join the control plane once and run modula-node.
set -eu

# Nested containers must not exceed this container's MTU (Docker Desktop uses
# less than 1500), and Docker Desktop's resolver hands out IPv6-only answers.
mtu=$(cat /sys/class/net/eth0/mtu)
host_ip=$(getent ahostsv4 host.docker.internal | awk 'NR==1{print $1}')
[ -n "$host_ip" ] && echo "$host_ip host.docker.internal" >>/etc/hosts
echo "nameserver 1.1.1.1" >/etc/resolv.conf
dockerd-entrypoint.sh dockerd --mtu "$mtu" --dns 1.1.1.1 >/var/log/dockerd.log 2>&1 &
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
