#!/usr/bin/env bash
# End-to-end install test: a main server and a worker, each a privileged
# systemd container on one Docker network, installed with deploy/install.sh
# from a locally built signed release (scripts/release.sh).
#
#   ARCHES=arm64 scripts/release.sh && deploy/e2e/run.sh      # keep: KEEP=1
set -euo pipefail
cd "$(dirname "$0")/../.."

# shellcheck source=deploy/e2e/lib.sh
. deploy/e2e/lib.sh

cleanup() {
  [[ -n ${KEEP:-} ]] && { echo "kept: docker exec -it e2e-main bash"; return; }
  docker rm -f e2e-main e2e-worker >/dev/null 2>&1 || true
  docker volume rm e2e-main-docker e2e-worker-docker e2e-main-containerd e2e-worker-containerd >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
}
trap cleanup EXIT

build_image

echo "main server"
boot e2e-main
main_ip=$(ip_of e2e-main)
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

echo "main-server migration (install.sh main --restore, bundle built by hand)"
worker_ip=$(ip_of e2e-worker)
bundle() { # bundle MIGRATION_ID — a promotion bundle from e2e-main, into e2e-worker
  on e2e-main bash -c "set -e; rm -rf /root/b && mkdir -p /root/b/uploads && cd /root/b
    runuser -u postgres -- pg_dump --format=custom -d modula >control-plane.dump
    cp /var/lib/modula/master.key . && echo upload >uploads/e2e.tar.gz
    printf 'MIGRATION_ID=%s\nPUBLIC_URL=https://$DOMAIN\nNODE_URL=https://$worker_ip:7101\nNODE_PORT=7101\nACME_EMAIL=ops@e2e.test\nVERSION=$VERSION\n' '$1' >promotion.env
    tar -cf /root/bundle.tar promotion.env control-plane.dump master.key uploads"
  docker exec e2e-main cat /root/bundle.tar | docker exec -i e2e-worker sh -c 'mkdir -p /var/lib/modula-node/promote && cat >/var/lib/modula-node/promote/bundle.tar'
}
promote() { # as modula-node runs it
  on e2e-worker bash -c "rm -f /root/promote.log; systemd-run --unit=modula-promote-e2e --collect --wait --quiet \
    -p StandardOutput=append:/root/promote.log -p StandardError=append:/root/promote.log --setenv=NO_COLOR=1 $* \
    /bin/bash /root/install.sh main --restore /var/lib/modula-node/promote/bundle.tar"
}
bundle "bad id!"
promote && fail "a bundle with a bad MIGRATION_ID was accepted"
on e2e-worker grep -q "MIGRATION_ID is missing or malformed" /root/promote.log && pass "--restore refuses a malformed promotion.env" || fail "no clear error for a bad bundle"
bundle 0000000000e2e001
promote --setenv=MODULA_INSTALL_DRY_RUN=1 && on e2e-worker grep -q "Dry run" /root/promote.log \
  && on e2e-worker test ! -e /etc/modula/server.env && pass "--restore dry run changes nothing" || fail "--restore dry run"
node_pid=$(on e2e-worker systemctl show -p MainPID --value modula-node)
promote || { on e2e-worker cat /root/promote.log >&2; fail "promotion failed"; }
on e2e-worker curl -fsS -o /dev/null http://127.0.0.1:7100/api/health && pass "the promoted server's control plane is healthy" || fail "promoted control plane is not healthy"
on e2e-worker grep -qx MODULA_PROMOTED_MIGRATION=0000000000e2e001 /etc/modula/server.env && pass "server.env names the migration" || fail "no MODULA_PROMOTED_MIGRATION"
docker exec e2e-main cat /var/lib/modula/master.key | docker exec -i e2e-worker cmp -s /var/lib/modula/master.key - \
  && pass "the instance's master key moved over" || fail "master key differs"
on e2e-worker test ! -e /var/lib/modula-node/promote/bundle.tar && pass "the bundle is gone after success" || fail "bundle left behind"
[[ $(on e2e-worker systemctl show -p MainPID --value modula-node) == "$node_pid" ]] && pass "modula-node kept running" || fail "modula-node restarted"
bundle 0000000000e2e001
promote && pass "a re-run with the same bundle resumes" || { on e2e-worker cat /root/promote.log >&2; fail "re-run failed"; }
bundle 0000000000e2e002
promote && fail "a second migration onto a control plane was accepted"
on e2e-worker grep -q "already runs a control plane" /root/promote.log && pass "--restore refuses a server that runs a control plane" || fail "unclear refusal"

echo "all e2e checks passed"
