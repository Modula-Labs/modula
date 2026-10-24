#!/usr/bin/env bash
# End-to-end install test: a main server and a worker, each a privileged
# systemd container on one Docker network, installed with deploy/install.sh
# from a locally built signed release (scripts/release.sh).
#
#   ARCHES=arm64 scripts/release.sh && deploy/e2e/run.sh      # keep: KEEP=1
set -euo pipefail
cd "$(dirname "$0")/../.."

RELEASE=${RELEASE:-dist/release}
NET=modula-e2e
DOMAIN=deploy.e2e.test
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[[ -f $RELEASE/SHA256SUMS.sig ]] || { echo "no release in $RELEASE; run scripts/release.sh" >&2; exit 1; }
RELEASE=$(cd "$RELEASE" && pwd)
# The published layout: binaries by checksum under files/, as publish.sh stores them.
FILES=$PWD/dist/e2e-files
rm -rf "$FILES" && mkdir -p "$FILES"
while read -r sum name; do
  ln -f "$RELEASE/${name#\*}" "$FILES/$sum" 2>/dev/null || cp "$RELEASE/${name#\*}" "$FILES/$sum"
done <"$RELEASE/SHA256SUMS"

pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ $*" >&2; exit 1; }
cleanup() {
  [[ -n ${KEEP:-} ]] && { echo "kept: docker exec -it e2e-main bash"; return; }
  docker rm -f e2e-main e2e-worker >/dev/null 2>&1 || true
  docker volume rm e2e-main-docker e2e-worker-docker >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
}
trap cleanup EXIT

docker build -q -t modula-e2e-ubuntu -f deploy/e2e/ubuntu-systemd.Dockerfile deploy/e2e >/dev/null
docker network create "$NET" >/dev/null 2>&1 || true

boot() { # boot NAME
  docker rm -f "$1" >/dev/null 2>&1 || true
  docker run -d --name "$1" --hostname "$1" --network "$NET" \
    --privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
    --tmpfs /run --tmpfs /run/lock \
    -v "$1-docker:/var/lib/docker" \
    -v "$RELEASE:/srv/releases/download/v$VERSION:ro" \
    -v "$FILES:/srv/releases/files:ro" \
    -v "$PWD/deploy/install.sh:/root/install.sh:ro" \
    modula-e2e-ubuntu >/dev/null
  for _ in $(seq 1 50); do
    docker exec "$1" systemctl is-system-running 2>/dev/null | grep -qE "running|degraded" && return 0
    sleep 0.2
  done
  fail "$1: systemd did not start"
}
on() { docker exec "$@"; }
env_vars=(-e MODULA_RELEASE_URL=file:///srv/releases -e MODULA_VERSION="$VERSION")

echo "main server"
boot e2e-main
main_ip=$(docker inspect -f "{{(index .NetworkSettings.Networks \"$NET\").IPAddress}}" e2e-main)
on e2e-main sh -c "echo '127.0.0.1 $DOMAIN' >> /etc/hosts"
on "${env_vars[@]}" e2e-main bash /root/install.sh --domain "$DOMAIN" --email ops@e2e.test --apps-domain apps.e2e.test

on e2e-main curl -fsS http://127.0.0.1:7100/api/instance | grep -q '"setupRequired":true' && pass "dashboard API answers, setup pending"
nodes() { on e2e-main runuser -u postgres -- psql -d modula -tAc "SELECT count(*) FROM nodes WHERE status = 'online'"; }
for _ in $(seq 1 50); do [[ $(nodes) -ge 1 ]] && break; sleep 0.2; done
[[ $(nodes) -ge 1 ]] && pass "the main server joined itself" || fail "local node is not online"

routes() { on e2e-main curl -fsS --unix-socket /run/modula-caddy/admin.sock http://localhost/config/apps/http/servers/modula/routes 2>/dev/null || true; }
for _ in $(seq 1 50); do routes | grep -q "$DOMAIN" && break; sleep 0.2; done
routes | grep -q "$DOMAIN" && pass "Caddy routes $DOMAIN to the dashboard" || fail "dashboard route missing"

on e2e-main systemctl restart modula-caddy
t=$(date +%s)
for _ in $(seq 1 50); do routes | grep -q "$DOMAIN" && break; sleep 0.2; done
routes | grep -q "$DOMAIN" && pass "config restored $(( $(date +%s) - t ))s after a Caddy restart" || fail "config not restored after Caddy restart"

on e2e-main test -f /root/modula-recovery-kit.txt && [[ $(on e2e-main stat -c %a /root/modula-recovery-kit.txt) == 600 ]] \
  && pass "recovery kit written (0600)" || fail "recovery kit missing"
on e2e-main stat -c '%U %a' /etc/modula/server.env | grep -q "root 640" && pass "server.env is root:modula 0640"
on e2e-main sh -c "ss -ltn | grep -E ':(5432|6379|7100) ' | grep -v -E '127.0.0.1|\[::1\]'" && fail "a private port listens publicly" || pass "Postgres, Redis and the API listen on loopback only"
on e2e-main iptables -S DOCKER-USER | grep -q -- "-d 169.254.169.254/32 -j DROP" \
  && pass "containers can't reach the metadata service (DOCKER-USER drop)" || fail "no metadata drop in DOCKER-USER"
[[ $(on e2e-main systemctl show modula-node -p PrivateTmp) == PrivateTmp=yes ]] \
  && pass "modula-node runs with PrivateTmp" || fail "modula-node is not sandboxed"
[[ $(on e2e-main systemctl show modula-node -p OOMScoreAdjust) == OOMScoreAdjust=-500 ]] \
  && pass "modula-node is spared by the OOM killer" || fail "modula-node OOMScoreAdjust missing"
on e2e-main id modula-build >/dev/null && pass "modula-build user exists" || fail "no modula-build user"
docker exec -i e2e-main cmp -s /etc/modula/release.pub.pem - <deploy/release.pub.pem \
  && pass "release key stored for upgrades" || fail "/etc/modula/release.pub.pem differs from deploy/release.pub.pem"
on e2e-main grep -q PUBKEY /etc/modula/upgrade.env && fail "upgrade.env carries a release key" || pass "upgrade.env names no release key"

echo "upgrade.sh (verified installer, same version)"
on e2e-main rm -f /var/lib/modula-upgrade/result
on e2e-main sh -c "echo $VERSION > /var/lib/modula-upgrade/request"
for _ in $(seq 1 300); do on e2e-main test -f /var/lib/modula-upgrade/result && break; sleep 0.2; done
on e2e-main grep -qx status=succeeded /var/lib/modula-upgrade/result \
  && pass "upgrade.sh verified and ran the release's install.sh" \
  || { on e2e-main cat /var/lib/modula-upgrade/result >&2 || true; fail "upgrade.sh failed"; }
# A tampered installer is refused.
on e2e-main sh -c "d=/tmp/tampered/download/v$VERSION && mkdir -p \$d && cd /srv/releases/download/v$VERSION \
  && cp SHA256SUMS SHA256SUMS.sig install.sh \$d/ && echo '# tampered' >> \$d/install.sh \
  && sed -i 's|^MODULA_RELEASE_URL=.*|MODULA_RELEASE_URL=file:///tmp/tampered|' /etc/modula/upgrade.env"
on e2e-main rm -f /var/lib/modula-upgrade/result
on e2e-main sh -c "echo $VERSION > /var/lib/modula-upgrade/request"
for _ in $(seq 1 300); do on e2e-main test -f /var/lib/modula-upgrade/result && break; sleep 0.2; done
on e2e-main grep -q "does not match SHA256SUMS" /var/lib/modula-upgrade/result \
  && pass "upgrade.sh refuses a tampered installer" || fail "a tampered installer was not refused"
on e2e-main sh -c "sed -i 's|^MODULA_RELEASE_URL=.*|MODULA_RELEASE_URL=file:///srv/releases|' /etc/modula/upgrade.env"

echo "re-run (idempotent)"
on "${env_vars[@]}" e2e-main bash /root/install.sh --domain "$DOMAIN" --email ops@e2e.test --apps-domain apps.e2e.test | tail -3

echo "upgrade (same version)"
on "${env_vars[@]}" e2e-main bash /root/install.sh upgrade | tail -2

echo "worker"
boot e2e-worker
on e2e-worker sh -c "echo '$main_ip $DOMAIN' >> /etc/hosts"
join=$(on e2e-main sh -c 'set -a; . /etc/modula/server.env; set +a; runuser -u modula -- modula-server join-token --ttl-minutes 5' 2>/dev/null)
args=$(sed -E 's/.* join (--server .*)/\1/' <<<"$join")
# The token in the environment rather than on the command line.
token=$(grep -oE 'mcj_[A-Za-z0-9_-]+' <<<"$args")
args=$(sed -E 's/--token [^ ]+ ?//' <<<"$args")
# shellcheck disable=SC2086 # args is a list of flags
on "${env_vars[@]}" -e MODULA_JOIN_TOKEN="$token" e2e-worker bash /root/install.sh join $args --name e2e-worker
for _ in $(seq 1 50); do [[ $(nodes) -ge 2 ]] && break; sleep 0.2; done
[[ $(nodes) -ge 2 ]] && pass "worker joined over mTLS" || fail "worker is not online"
on e2e-worker iptables -S DOCKER-USER | grep -q -- "-d 169.254.169.254/32 -j DROP" \
  && pass "worker: metadata drop in DOCKER-USER" || fail "worker: no metadata drop"

echo "all e2e checks passed"
