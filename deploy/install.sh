#!/usr/bin/env bash
# Modula Deploy installer for Ubuntu 22.04+/Debian 12+ (amd64, arm64).
#
#   Main server (dashboard + the first server for apps):
#     curl -fsSL https://usemodula.org/install.sh | sudo bash -s -- --domain deploy.example.com --email you@example.com
#
#   Another server, joining an instance (the dashboard prints this line):
#     curl -fsSL https://deploy.example.com/api/install.sh | sudo bash -s -- join --server … --token … --ca …
#
#   Upgrade whatever is installed here:
#     curl -fsSL https://usemodula.org/install.sh | sudo bash -s -- upgrade [--version X.Y.Z]
#
# Nothing is compiled and nothing comes from npm: prebuilt static binaries,
# checked against SHA-256 sums signed with the release key below. Postgres,
# Redis and (if missing) Docker come from the distribution's own packages.
# Every step is idempotent; re-running resumes where a failed run stopped.
set -euo pipefail

# Filled in when an instance serves this script, so joining servers install
# the instance's own version and fetch binaries from it first.
VERSION="${MODULA_VERSION:-__MODULA_VERSION__}"
SERVER_RELEASES="__MODULA_SERVER_RELEASES__"
RELEASE_URL="${MODULA_RELEASE_URL:-https://downloads.usemodula.org/releases}"
# Ed25519 key that signs SHA256SUMS (deploy/release.pub.pem).
RELEASE_PUBKEY="${MODULA_RELEASE_PUBKEY:-MCowBQYDK2VwAyEAEofOHNvogrpURvg/JDfOV83WE6+yAoXsdXvjJlOp/xI=}"

PREFIX=/opt/modula
CONF=/etc/modula
STATE=/var/lib/modula
NODE_STATE=/var/lib/modula-node
CADDY_STATE=/var/lib/modula-caddy
CADDY_SOCKET=/run/modula-caddy/admin.sock
KEEP_RELEASES=3

# ---------------------------------------------------------------- output
# Teal on terminals (24-bit when the terminal says so, else 256 colors); plain
# lines with no cursor tricks when piped or logged (cloud-init), or NO_COLOR.

A="" B="" D="" R="" N="" TTY=0 STEP_LOG="" STEP_LABEL=""
if [[ -t 1 && ${TERM:-} != dumb ]]; then
  TTY=1
  if [[ -z ${NO_COLOR:-} ]]; then
    case ${COLORTERM:-} in
      truecolor | 24bit) A=$'\e[38;2;45;212;191m' ;;
      *) A=$'\e[38;5;43m' ;;
    esac
    B=$'\e[1m' D=$'\e[2m' R=$'\e[31m' N=$'\e[0m'
  fi
fi
T0=$(date +%s%N)
ms_since() { echo $(( ($(date +%s%N) - $1) / 1000000 )); }
fmt_ms() { printf "%d.%ds" $(( $1 / 1000 )) $(( $1 % 1000 / 100 )); }
# die "what went wrong" ["what to do about it"]
# Inside a step, the step's own ✗ line heads the message.
die() {
  if [[ -n $STEP_LOG ]]; then
    printf '%s%s%s\n' "$R" "$1" "$N" >&2
    [[ -n ${2:-} ]] && printf '%s\n' "$2" >&2
  else
    printf '  %s✗ %s%s\n' "$R" "$1" "$N" >&2
    [[ -n ${2:-} ]] && printf '    %s\n' "$2" >&2
  fi
  exit 1
}
note() { printf '  %s%s%s\n' "$D" "$*" "$N"; }
header() { # header "greeting"
  printf '\n  %s%s◆ modula%s  %sdeploy · your own little cloud%s\n' "$A" "$B" "$N" "$D" "$N"
  printf '  %s\n\n' "$1"
}
card_top() { printf '\n  %s╭─%s %s%s%s\n' "$A" "$N" "$B" "$1" "$N"; }
card() { if [[ -n $* ]]; then printf '  %s│%s  %s\n' "$A" "$N" "$*"; else printf '  %s│%s\n' "$A" "$N"; fi; }
card_kv() { printf '  %s│%s  %-14s%s\n' "$A" "$N" "$1" "$2"; }
card_end() { printf '  %s╰─%s %s\n\n' "$A" "$N" "$1"; }

# step "label" fn args… — runs one step and prints how long it took. Steps run
# in this shell (so they can set variables) with errexit on; if one fails, the
# EXIT trap prints its output. A step that exits (die) does so with its output
# still redirected, so the trap writes to copies of the original stdout and
# stderr (fds 3 and 4).
exec 3>&1 4>&2
step() {
  STEP_LABEL=$1; shift
  local t; t=$(date +%s%N)
  [[ $TTY == 1 ]] && printf '  %s→%s %s…' "$D" "$N" "$STEP_LABEL"
  STEP_LOG=$(mktemp)
  "$@" >"$STEP_LOG" 2>&1
  [[ $TTY == 1 ]] && printf '\r\e[K'
  printf '  %s✓%s %-40s %s%s%s\n' "$A" "$N" "$STEP_LABEL" "$D" "$(fmt_ms "$(ms_since "$t")")" "$N"
  rm -f "$STEP_LOG"
  STEP_LOG=""
}
on_exit() {
  local rc=$?
  if [[ $rc -ne 0 && -n $STEP_LOG ]]; then
    [[ $TTY == 1 ]] && printf '\r\e[K' >&3
    printf '  %s✗ %s%s\n' "$R" "$STEP_LABEL" "$N" >&3
    sed 's/^/    /' "$STEP_LOG" >&4
    rm -f "$STEP_LOG"
    printf '\n  %s\n\n' "Once that's sorted, run the same command again; finished steps are skipped." >&4
  fi
}
trap on_exit EXIT

usage() {
  sed -n '2,15p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//' || true
  cat <<'EOF'
Options (main server):
  --domain HOST        dashboard hostname, e.g. deploy.example.com (DNS must point here)
  --email ADDRESS      contact for Let's Encrypt
  --apps-domain HOST   parent for generated app domains, e.g. apps.example.com
                       (needs a wildcard DNS record *.apps.example.com → this server)
  --version X.Y.Z      install this version instead of the latest
Options (join):
  --server URL --token TOKEN --ca FINGERPRINT [--name NAME]
EOF
}

# ---------------------------------------------------------------- arguments

MODE=main DOMAIN="" EMAIL="" APPS_DOMAIN="" JOIN_SERVER="" JOIN_TOKEN="" JOIN_CA="" NODE_NAME=""
case "${1:-}" in
  join | upgrade) MODE=$1; shift ;;
  main) shift ;;
esac
while [[ $# -gt 0 ]]; do
  case "$1" in
    --domain) DOMAIN=${2:?}; shift 2 ;;
    --email) EMAIL=${2:?}; shift 2 ;;
    --apps-domain) APPS_DOMAIN=${2:?}; shift 2 ;;
    --version) VERSION=${2:?}; shift 2 ;;
    --server) JOIN_SERVER=${2:?}; shift 2 ;;
    --token) JOIN_TOKEN=${2:?}; shift 2 ;;
    --ca) JOIN_CA=${2:?}; shift 2 ;;
    --name) NODE_NAME=${2:?}; shift 2 ;;
    -h | --help) usage; exit 0 ;;
    *) usage >&2; die "unknown option $1" "The options are listed above; --help shows them again." ;;
  esac
done
[[ $VERSION == __* ]] && VERSION=latest
[[ $SERVER_RELEASES == __* ]] && SERVER_RELEASES=""

host_re='^([a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,63}$'
case $MODE in
  main)
    [[ -n $DOMAIN && -n $EMAIL ]] || { usage >&2; die "--domain and --email are required" "For example: … | sudo bash -s -- --domain deploy.example.com --email you@example.com"; }
    DOMAIN=${DOMAIN,,}; APPS_DOMAIN=${APPS_DOMAIN,,}
    [[ $DOMAIN =~ $host_re ]] || die "--domain must be a hostname like deploy.example.com" "Just the name, without https:// or a path."
    [[ -z $APPS_DOMAIN || $APPS_DOMAIN =~ $host_re ]] || die "--apps-domain must be a hostname like apps.example.com" "Just the name, without https:// or a path."
    [[ $EMAIL =~ ^[^@[:space:]]+@[^@[:space:]]+$ ]] || die "--email is not an email address" "Let's Encrypt uses it to tell you about expiring certificates."
    ;;
  join)
    [[ $JOIN_SERVER =~ ^https://[^[:space:]]+$ ]] || die "--server must be an https:// URL" "Copy the whole join command from the dashboard (Servers → Add server)."
    [[ $JOIN_TOKEN =~ ^mcj_[A-Za-z0-9_-]+$ ]] || die "--token must be a join token (mcj_…)" "Copy the whole join command from the dashboard (Servers → Add server)."
    [[ $JOIN_CA =~ ^sha256:[0-9a-f]{64}$ ]] || die "--ca must be sha256:<64 hex>" "Copy the whole join command from the dashboard (Servers → Add server)."
    [[ -z $NODE_NAME || $NODE_NAME =~ ^[A-Za-z0-9._-]{1,63}$ ]] || die "--name may use letters, digits, . _ -" "Up to 63 characters, e.g. --name web-1."
    ;;
esac

# ---------------------------------------------------------------- checks

preflight() {
  [[ $EUID -eq 0 ]] || die "this needs to run as root" "Try again with sudo: curl -fsSL … | sudo bash -s -- …"
  [[ -d /run/systemd/system ]] || die "systemd is required" "Modula runs its services with systemd; use a regular VM or server rather than a container."
  command -v apt-get >/dev/null || die "only Debian and Ubuntu are supported for now" "Ubuntu 22.04+ or Debian 12+ will do nicely."
  case $(uname -m) in
    x86_64) ARCH=amd64 ;;
    aarch64 | arm64) ARCH=arm64 ;;
    *) die "unsupported CPU architecture $(uname -m)" "Modula runs on amd64 (x86_64) and arm64 servers." ;;
  esac
  if [[ $MODE == main && -f $CONF/node.env && ! -f $CONF/server.env ]]; then
    die "this server already joined another instance" "To make it a main server, start from a fresh machine instead."
  fi
  if [[ $MODE == join && -f $CONF/server.env ]]; then
    die "this is a main server; it already runs modula-node" "It already runs apps for its own instance; no need to join it."
  fi
}

# ---------------------------------------------------------------- packages

need_docker() { ! command -v docker >/dev/null; }

packages() {
  local want=(ca-certificates curl openssl)
  [[ $MODE == main ]] && want+=(postgresql redis-server)
  need_docker && want+=(docker.io)
  local missing=()
  for p in "${want[@]}"; do
    dpkg-query -W -f='${Status}' "$p" 2>/dev/null | grep -q "ok installed" || missing+=("$p")
  done
  [[ ${#missing[@]} -eq 0 ]] && return 0
  export DEBIAN_FRONTEND=noninteractive
  # Only refresh package lists when they are missing or older than a day.
  if [[ -z $(find /var/lib/apt/lists -maxdepth 1 -name '*Packages*' -mtime -1 -print -quit 2>/dev/null) ]]; then
    apt-get update -qq
  fi
  apt-get install -y -qq --no-install-recommends "${missing[@]}"
  systemctl enable --now docker >/dev/null 2>&1 || true
}

# ---------------------------------------------------------------- release files

# artifact NAME DEST — the instance first (same version, close by), then the public release.
artifact() {
  local name=$1 dest=$2 url
  if [[ -n $SERVER_RELEASES ]] && curl -fsSL --retry 2 -o "$dest" "$SERVER_RELEASES$name" 2>/dev/null; then
    return 0
  fi
  if [[ $VERSION == latest ]]; then url="$RELEASE_URL/latest/download/$name"; else url="$RELEASE_URL/download/v$VERSION/$name"; fi
  curl -fsSL --retry 3 -o "$dest" "$url" || { echo "cannot download $url" >&2; return 1; }
}

# Downloads and verifies one release into $PREFIX/releases/<version>, sets RELEASE.
download() {
  local tmp; tmp=$(mktemp -d "$PREFIX/.download.XXXXXX")
  chmod 0755 "$tmp" # mktemp makes it 0700; modula-server must read and run it
  local names=("modula-node-linux-$ARCH" "caddy-linux-$ARCH" "buildctl-linux-$ARCH" "railpack-linux-$ARCH")
  [[ $MODE == main ]] && names+=("modula-server-linux-$ARCH")
  # Small files first; the binaries download in parallel.
  artifact SHA256SUMS "$tmp/SHA256SUMS"
  artifact SHA256SUMS.sig "$tmp/SHA256SUMS.sig"
  local pids=()
  for n in "${names[@]}"; do artifact "$n" "$tmp/$n" & pids+=($!); done
  for p in "${pids[@]}"; do wait "$p" || { rm -rf "$tmp"; return 1; }; done

  printf -- "-----BEGIN PUBLIC KEY-----\n%s\n-----END PUBLIC KEY-----\n" "$RELEASE_PUBKEY" >"$tmp/release.pub.pem"
  openssl pkeyutl -verify -pubin -inkey "$tmp/release.pub.pem" -rawin \
    -in "$tmp/SHA256SUMS" -sigfile "$tmp/SHA256SUMS.sig" >/dev/null \
    || { rm -rf "$tmp"; echo "SHA256SUMS signature is not valid; refusing to install" >&2; return 1; }
  (cd "$tmp" && sha256sum --check --ignore-missing --quiet SHA256SUMS) \
    || { rm -rf "$tmp"; echo "checksum mismatch; refusing to install" >&2; return 1; }
  for n in "${names[@]}"; do
    (cd "$tmp" && grep -q "  $n\$" SHA256SUMS) || { rm -rf "$tmp"; echo "$n is not in SHA256SUMS" >&2; return 1; }
    chmod 755 "$tmp/$n"
    ln -s "$n" "$tmp/${n%-linux-"$ARCH"}"
  done
  rm -f "$tmp/release.pub.pem"

  local v; v=$("$tmp/modula-node" --version | awk '{print $2}')
  [[ $v =~ ^[0-9A-Za-z.+-]+$ ]] || { rm -rf "$tmp"; echo "unexpected version string" >&2; return 1; }
  RELEASE=$PREFIX/releases/$v
  if [[ -d $RELEASE ]]; then rm -rf "$tmp"; else mv "$tmp" "$RELEASE"; fi
}

# Point $PREFIX/current at $RELEASE atomically.
activate() {
  ln -sfn "$RELEASE" "$PREFIX/.current.new"
  mv -T "$PREFIX/.current.new" "$PREFIX/current"
  for b in modula-node modula-server caddy; do
    [[ -e $PREFIX/current/$b ]] && ln -sfn "$PREFIX/current/$b" "/usr/local/bin/$b"
  done
  return 0
}

prune_releases() {
  local current; current=$(readlink -f "$PREFIX/current")
  # shellcheck disable=SC2012 # newest first; release names are version strings
  ls -1dt "$PREFIX"/releases/*/ 2>/dev/null | tail -n +$((KEEP_RELEASES + 1)) | while read -r d; do
    [[ $(readlink -f "$d") == "$current" ]] || rm -rf "$d"
  done
}

fetch_release() {
  mkdir -p "$PREFIX/releases"
  download
  [[ -L $PREFIX/current ]] || activate
}

# ---------------------------------------------------------------- users, files, units

secret() { openssl rand -hex 24; }

accounts() {
  id modula-caddy >/dev/null 2>&1 || useradd --system --home "$CADDY_STATE" --shell /usr/sbin/nologin modula-caddy
  install -d -m 0750 -o modula-caddy -g modula-caddy "$CADDY_STATE"
  install -d -m 0700 "$NODE_STATE"
  install -d -m 0755 "$CONF"
  if [[ $MODE == main ]]; then
    id modula >/dev/null 2>&1 || useradd --system --home "$STATE" --shell /usr/sbin/nologin modula
    install -d -m 0700 -o modula -g modula "$STATE"
  fi
}

database() {
  systemctl enable --now postgresql >/dev/null 2>&1
  # Peer authentication over the local socket: the modula OS user is the
  # modula database role, so there is no database password to leak.
  runuser -u postgres -- psql -qtAc "SELECT 1 FROM pg_roles WHERE rolname='modula'" | grep -q 1 \
    || runuser -u postgres -- psql -qc "CREATE ROLE modula LOGIN"
  runuser -u postgres -- psql -qtAc "SELECT 1 FROM pg_database WHERE datname='modula'" | grep -q 1 \
    || runuser -u postgres -- psql -qc "CREATE DATABASE modula OWNER modula"
}

redis() {
  local conf=/etc/redis/redis.conf
  if ! grep -q "^# modula" "$conf"; then
    # Loopback only, with a password; never published.
    printf "\n# modula\nbind 127.0.0.1 -::1\nprotected-mode yes\nrequirepass %s\n" "$(secret)" >>"$conf"
    systemctl restart redis-server
  fi
  systemctl enable redis-server >/dev/null 2>&1
}

server_config() {
  [[ -f $CONF/server.env ]] && return 0
  local redis_pass; redis_pass=$(awk '/^# modula/{m=1} m && /^requirepass /{print $2}' /etc/redis/redis.conf | tail -1)
  install -m 0600 /dev/null "$CONF/server.env"
  cat >"$CONF/server.env" <<EOF
# modula-server configuration (see README). Owned by root, readable by modula.
MODULA_DATABASE_URL=postgres://modula@localhost/modula?host=/var/run/postgresql
MODULA_REDIS_URL=redis://:$redis_pass@127.0.0.1:6379
MODULA_PUBLIC_URL=https://$DOMAIN
MODULA_LISTEN=127.0.0.1:7100
MODULA_MASTER_KEY_FILE=$STATE/master.key
MODULA_TRUSTED_PROXIES=127.0.0.1,::1
MODULA_NODE_LISTEN=0.0.0.0:7101
MODULA_EDGE_TLS=acme
MODULA_ACME_EMAIL=$EMAIL
MODULA_APPS_DOMAIN=$APPS_DOMAIN
MODULA_RELEASE_DIR=$PREFIX/current
MODULA_LOG=info,sqlx=warn
EOF
  chgrp modula "$CONF/server.env"
  chmod 0640 "$CONF/server.env"
}

master_key() {
  [[ -f $STATE/master.key ]] && return 0
  runuser -u modula -- "$PREFIX/current/modula-server" keygen --out "$STATE/master.key"
}

unit() { # unit NAME — writes the unit from stdin, reports whether it changed
  local path=/etc/systemd/system/$1.service new; new=$(cat)
  if [[ ! -f $path || $(cat "$path") != "$new" ]]; then
    printf "%s\n" "$new" >"$path"
    UNITS_CHANGED=1
  fi
}

units() {
  UNITS_CHANGED=0
  unit modula-caddy <<EOF
[Unit]
Description=Modula Deploy edge (Caddy, configured by modula-node)
StartLimitIntervalSec=0
After=network-online.target
Wants=network-online.target

[Service]
User=modula-caddy
Group=modula-caddy
Environment=XDG_DATA_HOME=$CADDY_STATE XDG_CONFIG_HOME=$CADDY_STATE
RuntimeDirectory=modula-caddy
RuntimeDirectoryMode=0750
# The admin API is a unix socket only modula-node (root) uses; the config it
# loads is never written to disk.
ExecStart=$PREFIX/current/caddy run --config $CONF/caddy.json
Restart=always
RestartSec=1
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
ReadWritePaths=$CADDY_STATE
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
EOF
  cat >"$CONF/caddy.json" <<EOF
{"admin":{"listen":"unix/$CADDY_SOCKET","config":{"persist":false}}}
EOF
  chmod 0644 "$CONF/caddy.json"

  local routes=""
  [[ $MODE == main || -f $CONF/server.env ]] && routes=" --route $(grep '^MODULA_PUBLIC_URL=' "$CONF/server.env" | sed 's|.*https://||')=127.0.0.1:7100"
  unit modula-node <<EOF
[Unit]
Description=Modula Deploy node (runs this server's workloads)
StartLimitIntervalSec=0
After=network-online.target docker.service modula-caddy.service
Wants=network-online.target docker.service modula-caddy.service

[Service]
# Manages Docker and Caddy, which is root-equivalent anyway.
EnvironmentFile=-$CONF/node.env
ExecStart=$PREFIX/current/modula-node --data-dir $NODE_STATE run --caddy-admin unix:$CADDY_SOCKET --buildctl $PREFIX/current/buildctl --railpack $PREFIX/current/railpack$routes
Restart=always
RestartSec=2
KillMode=mixed
TimeoutStopSec=10

[Install]
WantedBy=multi-user.target
EOF

  if [[ -f $CONF/server.env ]]; then
    unit modula-server <<EOF
[Unit]
Description=Modula Deploy control plane
StartLimitIntervalSec=0
After=network-online.target postgresql.service redis-server.service
Wants=network-online.target postgresql.service redis-server.service

[Service]
User=modula
Group=modula
EnvironmentFile=$CONF/server.env
ExecStart=$PREFIX/current/modula-server serve
Restart=always
RestartSec=2
TimeoutStopSec=10
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
CapabilityBoundingSet=
ReadWritePaths=$STATE

[Install]
WantedBy=multi-user.target
EOF
  fi
  [[ $UNITS_CHANGED == 1 ]] && systemctl daemon-reload
  return 0
}

wait_healthy() {
  for _ in $(seq 1 100); do
    curl -fsS -o /dev/null http://127.0.0.1:7100/api/health 2>/dev/null && return 0
    sleep 0.1
  done
  journalctl -u modula-server -n 30 --no-pager >&2 || true
  echo "modula-server did not become healthy" >&2
  return 1
}

start_main() {
  systemctl enable --now modula-caddy modula-server >/dev/null 2>&1
  wait_healthy
}

# Runs a modula-server subcommand with the installed configuration.
server_cli() {
  # shellcheck source=/dev/null
  (set -a; . "$CONF/server.env"; set +a; runuser -u modula -- "$PREFIX/current/modula-server" "$@")
}

join_local() {
  [[ -n $(ls -A "$NODE_STATE" 2>/dev/null) ]] && return 0 # already joined
  local cmd; cmd=$(server_cli join-token --ttl-minutes 5 2>/dev/null)
  local server token ca
  server=$(sed -E 's/.*--server ([^ ]+).*/\1/' <<<"$cmd")
  token=$(sed -E 's/.*--token ([^ ]+).*/\1/' <<<"$cmd")
  ca=$(sed -E 's/.*--ca ([^ ]+).*/\1/' <<<"$cmd")
  # This server dials itself over loopback; the certificate covers 127.0.0.1.
  "$PREFIX/current/modula-node" --data-dir "$NODE_STATE" join \
    --server "https://127.0.0.1:${server##*:}" --token "$token" --ca "$ca" --name "$(hostname -s)"
}

join_remote() {
  [[ -n $(ls -A "$NODE_STATE" 2>/dev/null) ]] && return 0 # already joined
  local args=(--server "$JOIN_SERVER" --token "$JOIN_TOKEN" --ca "$JOIN_CA")
  [[ -n $NODE_NAME ]] && args+=(--name "$NODE_NAME")
  "$PREFIX/current/modula-node" --data-dir "$NODE_STATE" join "${args[@]}"
}

start_node() {
  systemctl enable --now modula-caddy modula-node >/dev/null 2>&1
  systemctl is-active --quiet modula-node || { journalctl -u modula-node -n 30 --no-pager >&2; return 1; }
}

recovery_kit() {
  local kit=/root/modula-recovery-kit.txt
  [[ -f $kit ]] && return 0
  install -m 0600 /dev/null "$kit"
  {
    echo "Modula Deploy recovery kit"
    echo "Instance:  https://$DOMAIN"
    echo "Created:   $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo
    echo "This file holds the master key that encrypts every secret in this"
    echo "instance (variables, deployment snapshots, the internal CA)."
    echo "Without it, a database backup cannot be decrypted."
    echo
    echo "1. Copy this file somewhere safe and offline (a password manager)."
    echo "2. Delete it from this server:  shred -u $kit"
    echo
    echo "To restore: write the key below to $STATE/master.key (mode 0600,"
    echo "owner modula) on a fresh install before starting modula-server."
    echo
    echo "-----BEGIN MODULA MASTER KEY-----"
    base64 -w 64 "$STATE/master.key"
    echo "-----END MODULA MASTER KEY-----"
  } >"$kit"
}

# ---------------------------------------------------------------- modes

this_host() { hostname 2>/dev/null || uname -n; }

install_main() {
  header "Hey! Let's get Modula running on $(this_host). This takes a few minutes."
  step "Checking this server" preflight
  step "Installing packages" packages
  step "Downloading release ($VERSION)" fetch_release
  step "Creating users and directories" accounts
  step "Setting up Postgres" database
  step "Setting up Redis" redis
  step "Writing configuration" server_config
  step "Creating the master key" master_key
  step "Writing services" units
  step "Starting the control plane" start_main
  step "Joining this server" join_local
  step "Starting the node and edge" start_node
  step "Writing the recovery kit" recovery_kit
  local setup; setup=$(server_cli setup-token 2>/dev/null || true)
  card_top "All set! Modula is up and running  ${D}($(fmt_ms "$(ms_since "$T0")"))${N}"
  card ""
  card_kv "Dashboard" "${A}https://$DOMAIN${N}"
  if [[ -n $setup ]]; then
    card_kv "Setup token" "${B}$setup${N}"
    card_kv "" "${D}single use; create the first admin with it${N}"
  fi
  card_kv "Recovery kit" "/root/modula-recovery-kit.txt"
  card_kv "" "${R}copy it somewhere safe, then delete it from this server${N}"
  card ""
  card "${B}Next:${N} open ${A}https://$DOMAIN/setup${N}${setup:+ and paste the setup token}."
  card "${D}Point DNS for $DOMAIN${APPS_DOMAIN:+ and *.$APPS_DOMAIN} at this server, and open${N}"
  card "${D}ports 80 and 443 (plus 7101 for servers joining later).${N}"
  card ""
  card_end "Have fun shipping things!"
}

install_join() {
  header "Joining $JOIN_SERVER as a new node…"
  step "Checking this server" preflight
  step "Installing packages" packages
  step "Downloading release ($VERSION)" fetch_release
  step "Creating users and directories" accounts
  step "Writing services" units
  step "Joining" join_remote
  step "Starting the node and edge" start_node
  card_top "Welcome aboard!  ${D}($(fmt_ms "$(ms_since "$T0")"))${N}"
  card ""
  card "$(this_host) is now part of ${A}$JOIN_SERVER${N}."
  card "It'll show up on the Servers page in a moment, ready for apps."
  card ""
  card "${D}Open ports 80 and 443 for apps; the dashboard needs no inbound port.${N}"
  card ""
  card_end "Happy shipping!"
}

upgrade() {
  [[ -L $PREFIX/current ]] || die "Modula Deploy is not installed here" "Install it first: … | sudo bash -s -- --domain … --email …"
  if [[ -f $CONF/server.env ]]; then
    MODE=main
    SERVER_RELEASES="" # it would only hand back its own, current version
  fi
  local previous; previous=$(readlink -f "$PREFIX/current")
  header "Upgrading Modula on $(this_host) ${D}(currently $(basename "$previous"))${N}"
  step "Checking this server" preflight
  step "Downloading release ($VERSION)" download
  if [[ $(readlink -f "$RELEASE") == "$previous" ]]; then
    printf '\n  %s✓%s Already on %s%s%s; nothing to do. Enjoy!\n\n' "$A" "$N" "$B" "$(basename "$previous")" "$N"
    return 0
  fi
  step "Switching to $(basename "$RELEASE")" activate
  step "Writing services" units
  if [[ $MODE == main ]]; then
    systemctl restart modula-server
    if ! wait_healthy; then
      printf '  %s✗ The new version did not start; rolling back to %s.%s\n' "$R" "$(basename "$previous")" "$N" >&2
      RELEASE=$previous activate
      systemctl restart modula-server
      wait_healthy || true
      printf '    %s\n' "Your apps keep running. The log lines above should say what went wrong." >&2
      exit 1
    fi
  fi
  # The node reloads Caddy's config within seconds after Caddy restarts.
  step "Restarting the node and edge" systemctl restart modula-node modula-caddy
  prune_releases
  printf '\n  %s✓%s Upgraded to %s%s%s in %s. Fresh and ready!\n\n' "$A" "$N" "$B" "$(basename "$RELEASE")" "$N" "$(fmt_ms "$(ms_since "$T0")")"
}

case $MODE in
  main) install_main ;;
  join) install_join ;;
  upgrade) upgrade ;;
esac
