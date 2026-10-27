#!/usr/bin/env bash
# Modula Deploy installer for Ubuntu 22.04+/Debian 12+ (amd64, arm64).
#
#   Main server (dashboard + the first server for apps); asks for an email
#   and a server name, and for sudo when not run as root. Without --domain
#   the dashboard is served at https://<public IP>; add a domain later:
#     curl -fsSL https://usemodula.org/install.sh | bash
#
#   The same without questions (automation, cloud-init):
#     curl -fsSL https://usemodula.org/install.sh | sudo bash -s -- --email you@example.com [--domain deploy.example.com] [--name NAME]
#
#   Another server, joining an instance (the dashboard prints this line; the
#   token may also come in MODULA_JOIN_TOKEN, the Access secret in
#   MODULA_ACCESS_SECRET):
#     curl -fsSL https://deploy.example.com/api/install.sh | sudo bash -s -- join --server … --token … --ca …
#
#   Upgrade whatever is installed here (or from the dashboard; other servers
#   then follow the main server one at a time):
#     curl -fsSL https://usemodula.org/install.sh | sudo bash -s -- upgrade [--version X.Y.Z]
#
# Nothing is compiled and nothing comes from npm: prebuilt static binaries,
# checked against SHA-256 sums signed with the release key below. Postgres,
# Redis and (if missing) Docker come from the distribution's own packages.
# Every step is idempotent; re-running resumes where a failed run stopped.
#
# Installs (not upgrades) also set up the host: a firewall (ufw), automatic
# security updates, log caps, swap on small machines, clock sync and a few
# kernel limits. --no-harden skips all of that, --no-firewall just the firewall.
#
# Upgrades keep ports open (systemd socket activation): requests wait for the
# new process instead of failing. A failed health check rolls back (exit 3).
#
# MODULA_INSTALL_DRY_RUN=1 stops after the questions and root check, before
# anything is changed (for testing the installer itself).
#
# `main --restore BUNDLE` turns a server that already joined an instance into
# its main server: modula-node runs it (never by hand) when the control plane
# moves here. The bundle, from the old control plane, holds its settings
# (promotion.env), a database dump, the master key and uploaded sources. This
# installs and starts modula-server with them; modula-node keeps running,
# connected to the old control plane until that hands over.
#
# Everything runs from `main` on the last line, so a download cut short runs
# nothing at all.
set -euo pipefail

# Filled in when an instance serves this script, so joining servers install
# the instance's own version and fetch binaries from it first.
VERSION="${MODULA_VERSION:-__MODULA_VERSION__}"
SERVER_RELEASES="__MODULA_SERVER_RELEASES__"
# A verified copy of this script run by upgrade.sh gets the instance from it.
if [[ -n ${MODULA_SERVER_RELEASES:-} ]]; then SERVER_RELEASES=$MODULA_SERVER_RELEASES; fi
RELEASE_URL="${MODULA_RELEASE_URL:-https://downloads.usemodula.org/releases}"
# Ed25519 key that signs SHA256SUMS (deploy/release.pub.pem). The override is
# for development installs only; upgrades use $CONF/release.pub.pem.
RELEASE_PUBKEY="${MODULA_RELEASE_PUBKEY:-MCowBQYDK2VwAyEAVl1B0m1qDZY+gGHSLVry769Tl1KmGKKFhFiav9ccXCE=}"

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
  progress_fail "$1" "${2:-}"
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
# stderr (fds 3 and 4, opened by main).
step() {
  STEP_LABEL=$1; shift
  local t; t=$(date +%s%N)
  [[ $TTY == 1 ]] && printf '  %s→%s %s…' "$D" "$N" "$STEP_LABEL"
  progress_step "$STEP_LABEL" running
  STEP_LOG=$(mktemp)
  "$@" >"$STEP_LOG" 2>&1
  progress_step "$STEP_LABEL" "done"
  [[ $TTY == 1 ]] && printf '\r\e[K'
  printf '  %s✓%s %-40s %s%s%s\n' "$A" "$N" "$STEP_LABEL" "$D" "$(fmt_ms "$(ms_since "$t")")" "$N"
  rm -f "$STEP_LOG"
  STEP_LOG=""
}
skipped() { printf '  %s–%s %-40s %sskipped (%s)%s\n' "$D" "$N" "$1" "$D" "$2" "$N"; progress_step "$1" skipped; }
on_exit() {
  local rc=$?
  # The extracted bundle (master key included) never outlives the run; the
  # bundle itself stays after a failure so a re-run can resume.
  [[ -z $RESTORE_TMP ]] || rm -rf "$RESTORE_TMP"
  [[ $rc -ne 0 ]] && progress_fail "${STEP_LABEL:-The installer} failed" "The installer's output is in /var/log/cloud-init-output.log."
  progress_stop "$rc"
  if [[ $rc -ne 0 && -n $STEP_LOG ]]; then
    [[ $TTY == 1 ]] && printf '\r\e[K' >&3
    printf '  %s✗ %s%s\n' "$R" "$STEP_LABEL" "$N" >&3
    sed 's/^/    /' "$STEP_LOG" >&4
    rm -f "$STEP_LOG"
    printf '\n  %s\n\n' "Once that's sorted, run the same command again; finished steps are skipped." >&4
  fi
}

# ---------------------------------------------------------------- progress
# --progress-token (from `modula server create`): while the server has no
# dashboard yet, each step's state is served read-only on port 80 at
# /.modula-progress/<token>.json, so the CLI can show it without SSH. Only
# step names, states and error hints; never a secret. The token in the path
# (256 random bits) keeps it from strangers; index.html files stop listings.
# Served until right before Caddy takes port 80; after a failure for ten
# more minutes so the CLI sees it, then gone either way.

PROGRESS_TOKEN="" PROGRESS_ROOT=/run/modula-progress PROGRESS_PID="" PROGRESS_ON=0
PROGRESS_STEPS=() PROGRESS_STATE=running PROGRESS_ERROR=""
json_str() { local s=${1//\\/\\\\}; s=${s//\"/\\\"}; s=${s//$'\n'/ }; printf '"%s"' "$s"; }
progress_write() {
  [[ $PROGRESS_ON == 1 ]] || return 0
  local f=$PROGRESS_ROOT/.modula-progress/$PROGRESS_TOKEN.json steps=""
  [[ ${#PROGRESS_STEPS[@]} -gt 0 ]] && steps=$(IFS=,; printf '%s' "${PROGRESS_STEPS[*]}")
  printf '{"state":"%s","steps":[%s]%s}\n' "$PROGRESS_STATE" "$steps" "${PROGRESS_ERROR:+,\"error\":$PROGRESS_ERROR}" >"$f.tmp" \
    && chmod 0644 "$f.tmp" && mv -f "$f.tmp" "$f"
  return 0
}
# progress_step LABEL running|done|skipped
progress_step() {
  [[ $PROGRESS_ON == 1 ]] || return 0
  local entry; entry=$(printf '{"name":%s,"state":"%s","at":%s}' "$(json_str "$1")" "$2" "$(date +%s)")
  if [[ $2 == "done" && ${#PROGRESS_STEPS[@]} -gt 0 ]]; then
    PROGRESS_STEPS[${#PROGRESS_STEPS[@]}-1]=$entry
  else
    PROGRESS_STEPS+=("$entry")
  fi
  progress_write
}
# progress_fail MESSAGE [HINT] — the first failure is the one reported.
progress_fail() {
  [[ $PROGRESS_ON == 1 && $PROGRESS_STATE == running ]] || return 0
  PROGRESS_STATE=failed
  PROGRESS_ERROR=$(printf '{"step":%s,"message":%s,"hint":%s}' "$(json_str "$STEP_LABEL")" "$(json_str "$1")" "$(json_str "${2:-}")")
  progress_write
}
progress_start() {
  [[ -n $PROGRESS_TOKEN ]] && command -v python3 >/dev/null || return 0
  mkdir -p "$PROGRESS_ROOT/.modula-progress"
  chmod 0755 "$PROGRESS_ROOT" "$PROGRESS_ROOT/.modula-progress"
  : >"$PROGRESS_ROOT/index.html"
  : >"$PROGRESS_ROOT/.modula-progress/index.html"
  PROGRESS_ON=1
  progress_write
  python3 -m http.server 80 --bind 0.0.0.0 --directory "$PROGRESS_ROOT" </dev/null >/dev/null 2>&1 &
  PROGRESS_PID=$!
}
# progress_stop [EXIT_CODE] — right away, or ten minutes after a failure.
progress_stop() {
  [[ $PROGRESS_ON == 1 ]] || return 0
  PROGRESS_ON=0
  local pid=$PROGRESS_PID root=$PROGRESS_ROOT
  if [[ ${1:-0} -ne 0 && -n $pid ]]; then
    ( sleep 600; kill "$pid" 2>/dev/null; rm -rf "$root" ) </dev/null >/dev/null 2>&1 &
    disown || true
    return 0
  fi
  [[ -n $pid ]] && { kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; }
  rm -rf "$root"
}

usage() {
  sed -n '2,23p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//' || true
  cat <<'EOF'
Options (main server; asked for interactively when missing):
  --domain HOST        optional: a dashboard hostname that already points here. Without
                       it the dashboard is served at https://<public IP> (an IP
                       certificate) and you add a domain later in the dashboard
  --email ADDRESS      contact for Let's Encrypt
  --name NAME          this server's name on the Servers page (default: short hostname)
  --setup-token TOKEN  use this one-time setup token (mcsetup_…) instead of a random
                       one; `modula server create` passes it and finishes setup itself
  --progress-token T   serve install progress on port 80 at /.modula-progress/T.json
                       until the dashboard starts (for `modula server create`)
  --apps-domain HOST   parent for generated app domains, e.g. apps.example.com
                       (needs a wildcard DNS record *.apps.example.com → this server)
  --version X.Y.Z      install this version instead of the latest
  --restore BUNDLE     make this joined server the main server from the old one's
                       promotion bundle (modula-node runs this; never asks anything)
Options (join):
  --server URL --token TOKEN --ca FINGERPRINT [--name NAME]
                       (or the token in MODULA_JOIN_TOKEN instead of --token)
  --access-host HOST --access-id ID --access-secret SECRET
                       private mode: reach the instance through Cloudflare Access
                       (or the secret in MODULA_ACCESS_SECRET)
Host setup (main and join; upgrade never touches it):
  --no-harden          skip all host setup below
  --no-firewall        skip the firewall (ufw) only
  --auto-reboot        let security updates reboot at 04:00 when they need to
EOF
}

# ---------------------------------------------------------------- arguments

MODE=main DOMAIN="" EMAIL="" APPS_DOMAIN="" JOIN_SERVER="" JOIN_TOKEN="" JOIN_CA="" NODE_NAME=""
SETUP_TOKEN="" SETUP_TOKEN_HASH=""
# main --restore: the promotion bundle and what its promotion.env says.
RESTORE="" RESTORE_TMP="" MIGRATION_ID="" NODE_URL="" TCP_PROXY_HOST="" OLD_VERSION=""
HARDEN=1 FIREWALL=1 AUTO_REBOOT=0 HARDEN_ASKED=0 HARDEN_WHY=""
# The arguments to run this again with as root: all but the secrets, which
# travel in the environment (never on a command line others can read).
REEXEC_ARGS=()

parse_args() {
  # Secrets may come from the environment instead of flags.
  JOIN_TOKEN=${MODULA_JOIN_TOKEN:-}
  ACCESS_SECRET=${MODULA_ACCESS_SECRET:-}
  case "${1:-}" in
    join | upgrade) MODE=$1; REEXEC_ARGS+=("$1"); shift ;;
    main) REEXEC_ARGS+=("$1"); shift ;;
  esac
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --token) JOIN_TOKEN=${2:?}; shift 2; continue ;;
      --access-secret) ACCESS_SECRET=${2:?}; shift 2; continue ;;
    esac
    case "$1" in
      --no-harden | --no-firewall | --auto-reboot | -h | --help) REEXEC_ARGS+=("$1") ;;
      *) REEXEC_ARGS+=("$1" "${2:-}") ;;
    esac
    case "$1" in
      --domain) DOMAIN=${2:?}; shift 2 ;;
      --email) EMAIL=${2:?}; shift 2 ;;
      --apps-domain) APPS_DOMAIN=${2:?}; shift 2 ;;
      --version) VERSION=${2:?}; shift 2 ;;
      --server) JOIN_SERVER=${2:?}; shift 2 ;;
      --ca) JOIN_CA=${2:?}; shift 2 ;;
      --name) NODE_NAME=${2:?}; shift 2 ;;
      --access-host) ACCESS_HOST=${2:?}; shift 2 ;;
      --access-id) ACCESS_ID=${2:?}; shift 2 ;;
      --setup-token) SETUP_TOKEN=${2:?}; shift 2 ;;
      --restore) RESTORE=${2:?}; shift 2 ;;
      --progress-token) PROGRESS_TOKEN=${2:?}; shift 2 ;;
      --no-harden) HARDEN=0; HARDEN_ASKED=1; shift ;;
      --no-firewall) FIREWALL=0; shift ;;
      --auto-reboot) AUTO_REBOOT=1; shift ;;
      -h | --help) usage; exit 0 ;;
      *) usage >&2; die "unknown option $1" "The options are listed above; --help shows them again." ;;
    esac
  done
  [[ $VERSION == __* ]] && VERSION=latest
  [[ $SERVER_RELEASES == __* ]] && SERVER_RELEASES=""
  return 0
}

valid_host() { [[ ${1,,} =~ ^([a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,63}$ ]]; }
valid_ipv4() {
  local o='(0|[1-9][0-9]{0,2})'
  [[ $1 =~ ^$o\.$o\.$o\.$o$ ]] || return 1
  for o in "${BASH_REMATCH[@]:1}"; do ((o <= 255)) || return 1; done
}
# The dashboard's address: a hostname, or the public IPv4 without a domain.
valid_dashboard() { valid_host "$1" || valid_ipv4 "$1"; }
valid_email() { [[ $1 =~ ^[^@[:space:]]+@[^@[:space:]]+\.[^@[:space:]]+$ ]]; }
valid_name() { [[ $1 =~ ^[A-Za-z0-9._-]{1,63}$ ]]; }

# Answers to the main install's questions come from the terminal: when piped
# into bash, stdin is this script. PROMPT is set when they need asking.
PROMPT=0 PROMPT_FD=""
validate_args() {
  [[ -z $RESTORE || $MODE == main ]] || die "--restore only goes with main" "modula-node runs: install.sh main --restore BUNDLE"
  case $MODE in
    main)
      if [[ -n $RESTORE ]]; then
        # Everything else comes from the bundle (read once root can read it).
        [[ -z $DOMAIN$EMAIL$APPS_DOMAIN$SETUP_TOKEN$PROGRESS_TOKEN ]] \
          || die "--restore takes no --domain, --email, --apps-domain, --setup-token or --progress-token" "They come from the bundle's promotion.env."
        [[ -z $NODE_NAME ]] || valid_name "$NODE_NAME" || die "--name may use letters, digits, . _ -" "Up to 63 characters, e.g. --name web-1."
        return 0
      fi
      DOMAIN=${DOMAIN,,}; APPS_DOMAIN=${APPS_DOMAIN,,}
      [[ -z $DOMAIN ]] || valid_dashboard "$DOMAIN" || die "--domain must be a hostname like deploy.example.com, or this server's IPv4" "Just the name or address, without https:// or a path."
      [[ -z $APPS_DOMAIN ]] || valid_host "$APPS_DOMAIN" || die "--apps-domain must be a hostname like apps.example.com" "Just the name, without https:// or a path."
      [[ -z $EMAIL ]] || valid_email "$EMAIL" || die "--email is not an email address" "Let's Encrypt uses it to tell you about expiring certificates."
      [[ -z $NODE_NAME ]] || valid_name "$NODE_NAME" || die "--name may use letters, digits, . _ -" "Up to 63 characters, e.g. --name web-1."
      if [[ -n $SETUP_TOKEN ]]; then
        [[ $SETUP_TOKEN =~ ^mcsetup_[A-Za-z0-9_-]{43}$ ]] || die "--setup-token must be a setup token (mcsetup_…)" "modula server create generates it; leave it out to get a random one."
        # Only its hash reaches the server (MODULA_SETUP_TOKEN_HASH): the
        # same domain-separated SHA-256 as modula-server's token hashes.
        SETUP_TOKEN_HASH=$(printf 'modula/token/v1\0%s' "$SETUP_TOKEN" | sha256sum | awk '{print $1}')
      fi
      [[ -z $PROGRESS_TOKEN || $PROGRESS_TOKEN =~ ^[A-Za-z0-9_-]{32,64}$ ]] || die "--progress-token must be 32–64 letters, digits, _ or -" "modula server create generates it; leave it out otherwise."
      # No domain needed: the dashboard starts at https://<public IP> and a
      # domain is added later in the dashboard (or `modula instance domain`).
      if [[ -z $DOMAIN ]]; then
        DOMAIN=$(public_ip)
        [[ -n $DOMAIN ]] || die "couldn't find this server's public IPv4" "Pass it (or a domain that points here) with --domain."
      fi
      if [[ -z $EMAIL ]]; then
        if { exec 5</dev/tty; } 2>/dev/null; then
          PROMPT=1 PROMPT_FD=5
        else
          die "no terminal to ask for an email" "Pass it as a flag instead: … | sudo bash -s -- --email you@example.com [--name web-1]"
        fi
      fi
      ;;
    join)
      [[ $JOIN_SERVER =~ ^https://[^[:space:]]+$ ]] || die "--server must be an https:// URL" "Copy the whole join command from the dashboard (Servers → Add server)."
      [[ $JOIN_TOKEN =~ ^mcj_[A-Za-z0-9_-]+$ ]] || die "--token (or MODULA_JOIN_TOKEN) must be a join token (mcj_…)" "Copy the whole join command from the dashboard (Servers → Add server)."
      [[ $JOIN_CA =~ ^sha256:[0-9a-f]{64}$ ]] || die "--ca must be sha256:<64 hex>" "Copy the whole join command from the dashboard (Servers → Add server)."
      [[ -z $NODE_NAME ]] || valid_name "$NODE_NAME" || die "--name may use letters, digits, . _ -" "Up to 63 characters, e.g. --name web-1."
      [[ -z ${ACCESS_HOST:-} ]] || { valid_host "$ACCESS_HOST" && [[ ${ACCESS_ID:-} =~ ^[A-Za-z0-9._-]+$ && ${ACCESS_SECRET:-} =~ ^[A-Za-z0-9._-]+$ ]]; } \
        || die "--access-host, --access-id and --access-secret go together" "Copy the whole join command from the dashboard (Servers → Add server)."
      ;;
  esac
  return 0
}

# ---------------------------------------------------------------- root
# The one-liner runs as a regular user, so ask sudo to run this again as root.
# A piped script has no file to hand over: fetch it again from where it is
# served (an instance serves its own copy; the root of its release URL says where).
INSTALL_URL=""
install_url() {
  if [[ -n ${MODULA_INSTALL_URL:-} ]]; then
    INSTALL_URL=$MODULA_INSTALL_URL
  elif [[ -n $SERVER_RELEASES ]]; then
    INSTALL_URL="${SERVER_RELEASES%"/api/releases?file="}/api/install.sh"
  else
    INSTALL_URL=https://usemodula.org/install.sh
  fi
}

elevate() {
  [[ $EUID -eq 0 ]] && return 0
  command -v sudo >/dev/null || die "this needs root, and sudo isn't installed" "Run it again as root (su -, then the same command)."
  printf '\n  %s◆%s Modula needs root to install system services — asking sudo…\n' "$A" "$N"
  local self="" tmp="" v rc=0 keep=() secrets=() preserve=()
  if [[ -n ${BASH_SOURCE[0]:-} && -f ${BASH_SOURCE[0]} ]]; then
    self=${BASH_SOURCE[0]}
  else
    tmp=$(mktemp "${TMPDIR:-/tmp}/modula-install.XXXXXX")
    curl -fsSL --retry 2 -o "$tmp" "$INSTALL_URL" \
      || { rm -f "$tmp"; die "couldn't fetch the installer again from $INSTALL_URL" "Run it as root instead: curl -fsSL $INSTALL_URL | sudo bash${REEXEC_ARGS[*]:+ -s -- …}"; }
    self=$tmp
  fi
  # sudo starts with a clean environment; carry over what this run was given.
  # SSH_CONNECTION names the port this session came in on (kept open by the firewall).
  for v in MODULA_RELEASE_URL MODULA_RELEASE_PUBKEY MODULA_INSTALL_DRY_RUN MODULA_ALLOW_DOWNGRADE NO_COLOR COLORTERM TERM SSH_CONNECTION; do
    [[ -n ${!v:-} ]] && keep+=("$v=${!v}")
  done
  keep+=("MODULA_INSTALL_URL=$INSTALL_URL")
  [[ $VERSION != latest ]] && keep+=("MODULA_VERSION=$VERSION")
  # The join token and Access secret go through sudo's preserved environment,
  # not `env NAME=value` (which would put them on a command line).
  [[ -n $JOIN_TOKEN ]] && { export MODULA_JOIN_TOKEN=$JOIN_TOKEN; secrets+=(MODULA_JOIN_TOKEN); }
  [[ -n $ACCESS_SECRET ]] && { export MODULA_ACCESS_SECRET=$ACCESS_SECRET; secrets+=(MODULA_ACCESS_SECRET); }
  if [[ ${#secrets[@]} -gt 0 ]]; then
    preserve=("--preserve-env=$(IFS=,; printf '%s' "${secrets[*]}")")
    sudo "${preserve[@]}" true 2>/dev/null \
      || die "sudo won't pass the join token on to root" "Become root first (sudo -i, or su -), then run the same command."
  fi
  sudo "${preserve[@]}" env "${keep[@]}" bash "$self" "${REEXEC_ARGS[@]}" || rc=$?
  [[ -n $tmp ]] && rm -f "$tmp"
  exit "$rc"
}

# ---------------------------------------------------------------- checks

preflight() {
  [[ $EUID -eq 0 ]] || die "this needs to run as root" "Try again with sudo: curl -fsSL … | sudo bash"
  [[ -d /run/systemd/system ]] || die "systemd is required" "Modula runs its services with systemd; use a regular VM or server rather than a container."
  command -v apt-get >/dev/null || die "only Debian and Ubuntu are supported for now" "Ubuntu 22.04+ or Debian 12+ will do nicely."
  case $(uname -m) in
    x86_64) ARCH=amd64 ;;
    aarch64 | arm64) ARCH=arm64 ;;
    *) die "unsupported CPU architecture $(uname -m)" "Modula runs on amd64 (x86_64) and arm64 servers." ;;
  esac
  if [[ -n $RESTORE ]]; then
    restore_preflight
  elif [[ $MODE == main && -f $CONF/node.env && ! -f $CONF/server.env ]]; then
    die "this server already joined another instance" "To make it a main server, start from a fresh machine instead."
  fi
  if [[ $MODE == join && -f $CONF/server.env ]]; then
    die "this is a main server; it already runs modula-node" "It already runs apps for its own instance; no need to join it."
  fi
}

# ---------------------------------------------------------------- packages

need_docker() { ! command -v docker >/dev/null; }
installed() { dpkg-query -W -f='${Status}' "$1" 2>/dev/null | grep -q "ok installed"; }

# apt_install PKG… — installs whichever of them are missing.
apt_install() {
  local missing=() p
  for p in "$@"; do installed "$p" || missing+=("$p"); done
  [[ ${#missing[@]} -eq 0 ]] && return 0
  export DEBIAN_FRONTEND=noninteractive
  # Only refresh package lists when they are missing or older than a day.
  if [[ -z $(find /var/lib/apt/lists -maxdepth 1 -name '*Packages*' -mtime -1 -print -quit 2>/dev/null) ]]; then
    apt-get update -qq
  fi
  apt-get install -y -qq --no-install-recommends "${missing[@]}"
}

DOCKER_FRESH=0 # Docker was installed by this run, so nothing runs on it yet
packages() {
  # nftables + iproute2: the private network between servers (WireGuard is in the kernel).
  local want=(ca-certificates curl openssl jq nftables iproute2)
  [[ $MODE == main ]] && want+=(postgresql redis-server)
  if need_docker; then want+=(docker.io); DOCKER_FRESH=1; fi
  apt_install "${want[@]}"
  systemctl enable --now docker >/dev/null 2>&1 || true
}

# ---------------------------------------------------------------- host setup
# Optional (--no-harden), idempotent, and never touches sshd's configuration.
# Each step adds a phrase to SECURITY for the closing card and may queue
# a line in TIPS to print after it (step output itself is hidden).

SECURITY=() TIPS=()
NODE_PORT=7101 # MODULA_NODE_LISTEN in server_config

# The ports sshd listens on: its effective configuration, what it (or
# systemd's ssh.socket, Ubuntu 24.04+) listens on, and the port this session
# came in on. Never empty: 22 when nothing says otherwise.
ssh_ports() {
  local ports=() p
  if command -v sshd >/dev/null; then
    while read -r p; do ports+=("$p"); done < <(sshd -T 2>/dev/null | awk '$1 == "port" {print $2}')
  fi
  while read -r p; do ports+=("$p"); done < <(
    ss -Hltnp 2>/dev/null | awk '/"sshd"/ {sub(/.*:/, "", $4); print $4}'
    systemctl show -p Listen ssh.socket 2>/dev/null | grep -oE ':[0-9]+ \(Stream\)' | tr -dc '0-9\n'
  )
  # SSH_CONNECTION="client_ip client_port server_ip server_port"
  p=$(awk '{print $4}' <<<"${SSH_CONNECTION:-}")
  [[ -n $p ]] && ports+=("$p")
  [[ ${#ports[@]} -eq 0 ]] && ports=(22)
  printf '%s\n' "${ports[@]}" | grep -E '^[0-9]{1,5}$' | sort -nu
}

# (grep -q on a pipe would stop reading early: SIGPIPE fails it under pipefail.)
ufw_active() { command -v ufw >/dev/null && [[ $(ufw status 2>/dev/null) == "Status: active"* ]]; }

# What another firewall manager already in charge is called, or nothing.
# nftables counts when its service loads rules that drop or reject something
# (Debian's stock /etc/nftables.conf accepts everything).
other_firewall() {
  if systemctl is-active --quiet firewalld 2>/dev/null; then
    echo firewalld
  elif systemctl is-active --quiet nftables 2>/dev/null \
    && grep -qE '^[^#]*\<(drop|reject)\>' /etc/nftables.conf /etc/nftables.d/*.nft 2>/dev/null \
    && ! ufw_active; then
    echo nftables
  fi
  return 0
}

# Open ports for this kind of server, as ufw rules.
firewall_rules() {
  local p
  for p in $(ssh_ports); do echo "$p/tcp"; done
  echo 80/tcp 443/tcp 443/udp | tr ' ' '\n'
  # WireGuard between servers (modula-node --wg-port).
  echo 51820/udp
  [[ $MODE == main ]] && echo "$NODE_PORT/tcp"
  return 0
}

# Deny incoming except SSH (first, so this session survives) and Modula's
# ports. An active ufw keeps its rules and defaults; ours are only added.
# Docker containers here publish no ports (Caddy and modula-node's TCP proxies
# listen on the host), so Docker's own iptables chains don't bypass this.
# Docker also accepts its containers' forwarded traffic ahead of ufw, so
# ufw's FORWARD policy (DROP) stays as it is.
firewall() {
  local other; other=$(other_firewall)
  if [[ -n $other ]]; then
    TIPS+=("Firewall: $other is managing this server, so ufw was left alone. Allow $(firewall_rules | sed 's/:/-/' | paste -sd' ' | sed 's/ /, /g').")
    SECURITY+=("firewall left to $other")
    return 0
  fi
  apt_install ufw
  local active=0 rule ports=()
  ufw_active && active=1
  if [[ $active == 0 ]]; then
    ufw default deny incoming
    ufw default allow outgoing
  fi
  while read -r rule; do
    ufw allow "$rule" comment modula
    ports+=("${rule/:/-}")
  done < <(firewall_rules)
  [[ $active == 1 ]] || ufw --force enable
  ufw_active
  SECURITY+=("firewall on (${ports[*]}; app TCP ports as allocated)")
}

# Security updates only, installed daily; no reboots unless --auto-reboot.
# A host that already runs unattended-upgrades keeps its own settings.
UU_CONF=/etc/apt/apt.conf.d/52modula-unattended-upgrades
security_updates() {
  if installed unattended-upgrades && [[ ! -f $UU_CONF ]] \
    && grep -q '^APT::Periodic::Unattended-Upgrade "1"' <<<"$(apt-config dump 2>/dev/null)"; then
    SECURITY+=("automatic updates (already set up)")
    return 0
  fi
  apt_install unattended-upgrades
  cat >/etc/apt/apt.conf.d/20auto-upgrades <<'EOF'
APT::Periodic::Update-Package-Lists "1";
APT::Periodic::Unattended-Upgrade "1";
EOF
  local reboot=false
  [[ $AUTO_REBOOT == 1 ]] && reboot=true
  cat >"$UU_CONF" <<EOF
// Written by the Modula Deploy installer: security updates only.
#clear Unattended-Upgrade::Allowed-Origins;
#clear Unattended-Upgrade::Origins-Pattern;
Unattended-Upgrade::Origins-Pattern {
        "origin=Debian,codename=\${distro_codename}-security,label=Debian-Security";
        "origin=Debian,codename=\${distro_codename},label=Debian-Security";
        "origin=Ubuntu,archive=\${distro_codename}-security";
        "origin=UbuntuESMApps,archive=\${distro_codename}-apps-security";
        "origin=UbuntuESM,archive=\${distro_codename}-infra-security";
};
Unattended-Upgrade::Automatic-Reboot "$reboot";
Unattended-Upgrade::Automatic-Reboot-Time "04:00";
EOF
  systemctl enable --now unattended-upgrades >/dev/null 2>&1 || true
  if [[ $reboot == true ]]; then SECURITY+=("security updates (reboots at 04:00 if needed)"); else SECURITY+=("security updates"); fi
}

# Journald keeps at most 500 MB; Docker's own logs rotate at 3 × 10 MB.
# daemon.json is only ever written when absent, or merged with jq, and Docker
# restarts for it only when it was installed just now (nothing runs yet).
DOCKER_DAEMON=/etc/docker/daemon.json
DOCKER_LOGCFG='{"log-driver":"json-file","log-opts":{"max-size":"10m","max-file":"3"}}'
# Present when this installer wrote daemon.json itself (it may change it later).
DOCKER_OWNED=$CONF/docker-daemon.json.owned
docker_log_config() {
  local logcfg=$DOCKER_LOGCFG
  if [[ ! -s $DOCKER_DAEMON ]]; then
    install -d -m 0755 /etc/docker "$CONF"
    jq -n "$logcfg" >"$DOCKER_DAEMON" 2>/dev/null || printf '%s\n' "$logcfg" >"$DOCKER_DAEMON"
    touch "$DOCKER_OWNED"
  elif command -v jq >/dev/null && jq -e 'type == "object" and (has("log-driver") or has("log-opts") | not)' "$DOCKER_DAEMON" >/dev/null 2>&1; then
    local tmp; tmp=$(mktemp "$DOCKER_DAEMON.XXXXXX")
    jq ". + $logcfg" "$DOCKER_DAEMON" >"$tmp" && chmod 0644 "$tmp" && mv "$tmp" "$DOCKER_DAEMON"
  else
    return 1 # left as it is (its own log settings, or no jq to merge safely)
  fi
}

# Containers keep running while Docker itself restarts (security updates of
# docker.io): "live-restore": true. Only in a daemon.json this installer wrote
# itself or that is absent, never on swarm nodes. live-restore takes effect on a reload (SIGHUP),
# which leaves containers alone. Returns 1 when it left things as they were.
docker_live_restore() {
  command -v docker >/dev/null || return 1
  [[ $(docker info --format '{{.Swarm.LocalNodeState}}' 2>/dev/null) == active ]] && return 1
  if [[ ! -s $DOCKER_DAEMON ]]; then
    install -d -m 0755 /etc/docker "$CONF"
    printf '{"live-restore":true}\n' >"$DOCKER_DAEMON"
    touch "$DOCKER_OWNED"
  else
    command -v jq >/dev/null || return 1
    [[ -f $DOCKER_OWNED ]] || return 1
    jq -e 'has("live-restore") | not' "$DOCKER_DAEMON" >/dev/null 2>&1 || return 1
    local tmp; tmp=$(mktemp "$DOCKER_DAEMON.XXXXXX")
    jq '. + {"live-restore": true}' "$DOCKER_DAEMON" >"$tmp" && chmod 0644 "$tmp" && mv "$tmp" "$DOCKER_DAEMON"
  fi
  systemctl reload docker 2>/dev/null || systemctl kill -s HUP docker 2>/dev/null || true
}

logs() {
  local j=/etc/systemd/journald.conf.d/90-modula.conf
  local logcfg=$'[Journal]\nSystemMaxUse=500M'
  if [[ ! -f $j || $(cat "$j") != "$logcfg" ]]; then
    install -d -m 0755 "${j%/*}"
    printf '%s\n' "$logcfg" >"$j"
    systemctl restart systemd-journald
  fi
  if docker_log_config; then
    if [[ $DOCKER_FRESH == 1 ]]; then
      systemctl restart docker
    else
      # Modula's own containers rotate their logs already; this covers the rest.
      TIPS+=("Docker log rotation was added to $DOCKER_DAEMON for containers Modula doesn't manage; it applies from Docker's next restart.")
    fi
  fi
  SECURITY+=("logs capped")
  docker_live_restore && SECURITY+=("containers survive Docker restarts")
  return 0
}

# swap_wanted MEM_KB SWAP_KB — a 2 GiB swap file helps machines with less than
# 2 GiB of memory through builds; never when some swap is already on.
swap_wanted() { (( $1 < 2 * 1024 * 1024 && $2 == 0 )); }

swap() {
  local mem swp
  mem=$(awk '/^MemTotal:/ {print $2}' /proc/meminfo)
  swp=$(awk '/^SwapTotal:/ {print $2}' /proc/meminfo)
  swap_wanted "$mem" "$swp" || return 0
  if [[ ! -f /swapfile ]]; then
    local free; free=$(df -Pk / | awk 'NR == 2 {print $4}')
    if (( free < 4 * 1024 * 1024 )); then
      TIPS+=("This server has under 2 GiB of memory and no swap, and too little disk for a swap file; builds may run out of memory.")
      return 0
    fi
    # fallocate makes holes some filesystems (btrfs, some XFS) can't swap to.
    fallocate -l 2G /swapfile.new 2>/dev/null || dd if=/dev/zero of=/swapfile.new bs=1M count=2048 status=none
    chmod 0600 /swapfile.new
    mkswap /swapfile.new >/dev/null
    mv /swapfile.new /swapfile
  fi
  swapon /swapfile
  grep -qE '^/swapfile[[:space:]]' /etc/fstab || echo '/swapfile none swap sw 0 0' >>/etc/fstab
  echo 'vm.swappiness = 10' >/etc/sysctl.d/90-modula-swap.conf
  sysctl -q -w vm.swappiness=10
  SECURITY+=("2 GiB swap")
}

# TLS and the nodes' mutual TLS both need a correct clock.
time_sync() {
  local s
  for s in chrony chronyd ntp ntpsec systemd-timesyncd; do
    systemctl is-active --quiet "$s" 2>/dev/null && return 0
  done
  if ! systemctl cat systemd-timesyncd >/dev/null 2>&1; then
    # Debian splits it out; any other NTP daemon installed but stopped wins.
    installed chrony || installed ntpsec || installed ntp || apt_install systemd-timesyncd
  fi
  timedatectl set-ntp true 2>/dev/null || systemctl enable --now systemd-timesyncd
}

kernel() {
  cat >/etc/sysctl.d/90-modula.conf <<'EOF'
# Written by the Modula Deploy installer.
net.core.somaxconn = 4096
fs.inotify.max_user_watches = 524288
fs.inotify.max_user_instances = 1024
# Containers reach the internet through this host.
net.ipv4.ip_forward = 1
EOF
  sysctl --system >/dev/null
}

# Postgres and Redis must never listen beyond this machine.
local_only() {
  [[ $MODE == main ]] || return 0
  local exposed
  exposed=$(ss -Hltnp 2>/dev/null | awk '/"(postgres|redis-server)"/ && $4 !~ /^(127\.|\[::1\]|\[::ffff:127\.)/ {print $4}' | paste -sd' ' || true)
  [[ -z $exposed ]] && return 0
  TIPS+=("Postgres or Redis listens on $exposed, beyond this machine. Modula only needs them on localhost; check listen_addresses in postgresql.conf and bind in /etc/redis/redis.conf.")
}

# Never applied, only suggested: key-only SSH.
ssh_tip() {
  command -v sshd >/dev/null || return 0
  grep -qi '^passwordauthentication yes' <<<"$(sshd -T 2>/dev/null)" || return 0
  TIPS+=("SSH accepts passwords. Once your key works (ssh-copy-id), consider key-only logins:
      printf 'PasswordAuthentication no\nKbdInteractiveAuthentication no\n' | sudo tee /etc/ssh/sshd_config.d/10-key-only.conf
      sudo systemctl reload ssh")
}

harden() {
  if [[ $HARDEN == 0 ]]; then
    skipped "Host setup" "${HARDEN_WHY:---no-harden}"
    return 0
  fi
  if [[ $FIREWALL == 1 ]]; then step "Setting up the firewall" firewall; else skipped "Firewall" "--no-firewall"; fi
  step "Turning on security updates" security_updates
  step "Capping logs" logs
  step "Checking memory and swap" swap
  step "Syncing the clock" time_sync
  step "Tuning kernel limits" kernel
}

# Printed after the closing card.
print_tips() {
  local t
  [[ ${#TIPS[@]} -eq 0 ]] && return 0
  for t in "${TIPS[@]}"; do printf '  %s!%s %s\n' "$A" "$N" "$t"; done
  printf '\n'
}

# ---------------------------------------------------------------- release files

# artifact NAME DEST [SUMS] — the instance first (same version, close by), then
# the public release. With SUMS (a binary), from where the release stores it
# once by checksum: files/<sha256> as listed in SHA256SUMS (verified against
# its signature after). Without (the signed list and small files), by name.
artifact() {
  local name=$1 dest=$2 sums=${3:-} url sum
  if [[ -n $SERVER_RELEASES ]] && curl -fsSL --retry 2 -o "$dest" "$SERVER_RELEASES$name" 2>/dev/null; then
    return 0
  fi
  if [[ -n $sums ]]; then
    sum=$(awk -v n="$name" '$2 == n { print $1; exit }' "$sums")
    [[ $sum =~ ^[0-9a-f]{64}$ ]] || { echo "$name is not in SHA256SUMS" >&2; return 1; }
    url="$RELEASE_URL/files/$sum"
  elif [[ $VERSION == latest ]]; then
    url="$RELEASE_URL/latest/download/$name"
  else
    url="$RELEASE_URL/download/v$VERSION/$name"
  fi
  curl -fsSL --retry 3 -o "$dest" "$url" || { echo "cannot download $url" >&2; return 1; }
}

# Downloads and verifies one release into $PREFIX/releases/<version>, sets RELEASE.
download() {
  local tmp; tmp=$(mktemp -d "$PREFIX/.download.XXXXXX")
  chmod 0755 "$tmp" # mktemp makes it 0700; modula-server must read and run it
  local names=("modula-node-linux-$ARCH" "caddy-linux-$ARCH" "buildctl-linux-$ARCH" "railpack-linux-$ARCH" "wal-g-linux-$ARCH" "workerd-linux-$ARCH")
  [[ $MODE == main ]] && names+=("modula-server-linux-$ARCH")
  # Small files first; the binaries download in parallel.
  artifact SHA256SUMS "$tmp/SHA256SUMS"
  artifact SHA256SUMS.sig "$tmp/SHA256SUMS.sig"
  artifact VERSION "$tmp/VERSION"
  # The installer itself, so this server can hand it on.
  artifact install.sh "$tmp/install.sh"
  local pids=()
  for n in "${names[@]}"; do artifact "$n" "$tmp/$n" "$tmp/SHA256SUMS" & pids+=($!); done
  for p in "${pids[@]}"; do wait "$p" || { rm -rf "$tmp"; return 1; }; done

  printf -- "-----BEGIN PUBLIC KEY-----\n%s\n-----END PUBLIC KEY-----\n" "$RELEASE_PUBKEY" >"$tmp/release.pub.pem"
  openssl pkeyutl -verify -pubin -inkey "$tmp/release.pub.pem" -rawin \
    -in "$tmp/SHA256SUMS" -sigfile "$tmp/SHA256SUMS.sig" >/dev/null \
    || { rm -rf "$tmp"; echo "SHA256SUMS signature is not valid; refusing to install" >&2; return 1; }
  rm -f "$tmp/release.pub.pem"
  # sha256sum --ignore-missing skips files SHA256SUMS doesn't list, so every
  # file kept must be listed.
  listed() { grep -qE "^[0-9a-f]{64}  $1\$" "$tmp/SHA256SUMS"; }
  for n in VERSION install.sh "${names[@]}"; do
    listed "$n" || { rm -rf "$tmp"; echo "$n is not in SHA256SUMS" >&2; return 1; }
  done
  (cd "$tmp" && sha256sum --check --ignore-missing --quiet SHA256SUMS) \
    || { rm -rf "$tmp"; echo "checksum mismatch; refusing to install" >&2; return 1; }
  for n in "${names[@]}"; do
    chmod 755 "$tmp/$n"
    ln -s "$n" "$tmp/${n%-linux-"$ARCH"}"
  done

  # The signed VERSION says which release this is: the one asked for, and
  # never older than what runs here (an old, validly signed release served
  # as "latest" would otherwise roll this server back).
  local v; v=$(tr -d '[:space:]' <"$tmp/VERSION")
  [[ $v =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]+)?$ ]] || { rm -rf "$tmp"; echo "unexpected version string in VERSION" >&2; return 1; }
  if [[ $VERSION != latest && $v != "$VERSION" ]]; then
    rm -rf "$tmp"; echo "asked for $VERSION but the release is $v; refusing to install" >&2; return 1
  fi
  local bin_v; bin_v=$("$tmp/modula-node" --version | awk '{print $2}')
  [[ $bin_v == "$v" ]] || { rm -rf "$tmp"; echo "modula-node says $bin_v but the release is $v; refusing to install" >&2; return 1; }
  if [[ -L $PREFIX/current && -z ${MODULA_ALLOW_DOWNGRADE:-} ]]; then
    local cur; cur=$(basename "$(readlink -f "$PREFIX/current")")
    if [[ $(printf '%s\n%s\n' "$cur" "$v" | sort -V | tail -1) != "$v" ]]; then
      rm -rf "$tmp"
      echo "$v is older than the installed $cur; refusing to downgrade (MODULA_ALLOW_DOWNGRADE=1 allows it)" >&2
      return 1
    fi
  fi
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
  build_account
  if [[ $MODE == main ]]; then
    id modula >/dev/null 2>&1 || useradd --system --home "$STATE" --shell /usr/sbin/nologin modula
    install -d -m 0700 -o modula -g modula "$STATE"
    oom_dropins
  fi
}

# modula-node runs build planning (railpack, which reads the app's own
# repository) as this user rather than root. No login, no home of its own:
# the node gives it a HOME for each build.
build_account() {
  id modula-build >/dev/null 2>&1 \
    || useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin modula-build
  # And workerd, which runs the server's JavaScript functions, as its own.
  id modula-fn >/dev/null 2>&1 \
    || useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin modula-fn
}

# Under memory pressure the kernel kills app containers before the control
# plane and its databases. Takes effect when each service next (re)starts.
oom_dropins() {
  local u d changed=0 conf=$'# Written by the Modula Deploy installer.\n[Service]\nOOMScoreAdjust=-500'
  # postgresql.service is a wrapper on Debian/Ubuntu; the clusters run as postgresql@.
  for u in postgresql@.service redis-server.service; do
    d=/etc/systemd/system/$u.d
    if [[ ! -f $d/modula.conf || $(cat "$d/modula.conf") != "$conf" ]]; then
      install -d -m 0755 "$d"
      printf '%s\n' "$conf" >"$d/modula.conf"
      changed=1
    fi
  done
  [[ $changed == 0 ]] || systemctl daemon-reload
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
  # Written whole, then moved into place: a run cut short leaves none (or
  # the old one), never half of one.
  local new=$CONF/.server.env.new
  install -m 0600 /dev/null "$new"
  cat >"$new" <<EOF
# modula-server configuration (see README). Owned by root, readable by modula.
MODULA_DATABASE_URL=postgres://modula@localhost/modula?host=/var/run/postgresql
MODULA_REDIS_URL=redis://:$redis_pass@127.0.0.1:6379
MODULA_PUBLIC_URL=https://$DOMAIN
MODULA_LISTEN=127.0.0.1:7100
MODULA_MASTER_KEY_FILE=$STATE/master.key
MODULA_TRUSTED_PROXIES=127.0.0.1,::1
MODULA_NODE_LISTEN=0.0.0.0:$NODE_PORT
MODULA_EDGE_TLS=acme
MODULA_ACME_EMAIL=$EMAIL
MODULA_APPS_DOMAIN=$APPS_DOMAIN
MODULA_RELEASE_DIR=$PREFIX/current
MODULA_LOG=info,sqlx=warn
EOF
  [[ -z $SETUP_TOKEN_HASH ]] || echo "MODULA_SETUP_TOKEN_HASH=$SETUP_TOKEN_HASH" >>"$new"
  # A promoted server keeps the instance's node address, and the marker that
  # tells modula-server to finish the hand-over (and preflight that a re-run
  # resumes this very migration).
  if [[ -n $RESTORE ]]; then
    {
      echo "MODULA_NODE_URL=$NODE_URL"
      [[ -z $TCP_PROXY_HOST ]] || echo "MODULA_TCP_PROXY_HOST=$TCP_PROXY_HOST"
      echo "MODULA_PROMOTED_MIGRATION=$MIGRATION_ID"
    } >>"$new"
  fi
  chgrp modula "$new"
  chmod 0640 "$new"
  mv -f "$new" "$CONF/server.env"
}

master_key() {
  [[ -f $STATE/master.key ]] && return 0
  runuser -u modula -- "$PREFIX/current/modula-server" keygen --out "$STATE/master.key"
}

# unit NAME.TYPE — writes a systemd unit from stdin; records whether it changed.
CHANGED=" "
unit() {
  local path=/etc/systemd/system/$1 new; new=$(cat)
  if [[ ! -f $path || $(cat "$path") != "$new" ]]; then
    printf "%s\n" "$new" >"$path"
    CHANGED+="$1 "
  fi
}
changed() { [[ $CHANGED == *" $1 "* ]]; }

# no_unit NAME.TYPE — removes a unit this release doesn't use. Only a running
# one is stopped: stopping a socket also stops the service that requires it.
no_unit() {
  local path=/etc/systemd/system/$1
  [[ -f $path ]] || return 0
  if systemctl is-active --quiet "$1"; then
    systemctl disable --now "$1" >/dev/null 2>&1 || true
  else
    systemctl disable "$1" >/dev/null 2>&1 || true
  fi
  rm -f "$path"
  CHANGED+="$1 "
}

# A server.env value, or its default.
server_env() { local v; v=$(sed -n "s/^$1=//p" "$CONF/server.env" 2>/dev/null | tail -1); printf '%s' "${v:-$2}"; }

UPGRADE_DIR=/var/lib/modula-upgrade

# Rootless BuildKit creates a user namespace, which Ubuntu 23.10+ forbids
# unless an AppArmor profile allows it (kernel.apparmor_restrict_unprivileged_userns).
# Rather than lifting that host-wide, a profile only the build engine's
# container runs under; modula-node uses it when it is loaded.
buildkit_apparmor() {
  [[ -r /proc/sys/kernel/apparmor_restrict_unprivileged_userns ]] || return 0
  [[ -f /etc/apparmor.d/abi/4.0 ]] && command -v apparmor_parser >/dev/null || return 0
  cat >/etc/apparmor.d/modula-buildkit <<'EOF_AA'
# Modula: the build engine (rootless BuildKit) may create user namespaces.
abi <abi/4.0>,
include <tunables/global>

profile modula-buildkit flags=(unconfined) {
  userns,
}
EOF_AA
  apparmor_parser -r /etc/apparmor.d/modula-buildkit 2>/dev/null \
    || TIPS+=("Couldn't load the modula-buildkit AppArmor profile; builds may fail to start (docker logs modula-buildkit).")
}

# The key upgrade.sh checks the next installer against. Written by every
# install and upgrade (whose installer was itself verified), so a signed
# installer can rotate it.
RELEASE_KEY_FILE=$CONF/release.pub.pem
release_key() {
  install -d -m 0755 "$CONF"
  local tmp; tmp=$(mktemp "$CONF/.release.pub.pem.XXXXXX")
  printf -- "-----BEGIN PUBLIC KEY-----\n%s\n-----END PUBLIC KEY-----\n" "$RELEASE_PUBKEY" >"$tmp"
  chmod 0644 "$tmp"
  mv -f "$tmp" "$RELEASE_KEY_FILE"
}

# Containers never reach the cloud's metadata service (instance credentials,
# user data with join tokens): a DROP at the top of Docker's DOCKER-USER chain,
# put back whenever Docker (re)starts. Plain commands, no shell; the first
# line of each pair removes an existing copy so restarts don't stack rules.
# modula-node also re-asserts it at startup.
metadata_firewall_unit() {
  local ipt ip6t v6=""
  ipt=$(command -v iptables || true)
  ip6t=$(command -v ip6tables || true)
  if [[ -z $ipt ]]; then
    no_unit modula-firewall.service
    TIPS+=("iptables isn't installed, so containers can reach the cloud metadata service (169.254.169.254).")
    return 0
  fi
  # Docker only creates the IPv6 chain with IPv6 on, hence the "-" (ignore failure).
  if [[ -n $ip6t ]]; then
    v6=$'\n'"ExecStartPre=-$ip6t -w -D DOCKER-USER -d fd00:ec2::254/128 -j DROP"
    v6+=$'\n'"ExecStart=-$ip6t -w -I DOCKER-USER -d fd00:ec2::254/128 -j DROP"
    v6+=$'\n'"ExecStop=-$ip6t -w -D DOCKER-USER -d fd00:ec2::254/128 -j DROP"
  fi
  unit modula-firewall.service <<EOF
[Unit]
Description=Modula Deploy: keep containers away from the cloud metadata service
After=docker.service
PartOf=docker.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStartPre=-$ipt -w -D DOCKER-USER -d 169.254.169.254/32 -j DROP
ExecStart=$ipt -w -I DOCKER-USER -d 169.254.169.254/32 -j DROP
ExecStop=-$ipt -w -D DOCKER-USER -d 169.254.169.254/32 -j DROP$v6

[Install]
WantedBy=multi-user.target docker.service
EOF
}

metadata_firewall_start() {
  [[ -f /etc/systemd/system/modula-firewall.service ]] || return 0
  systemctl enable modula-firewall.service >/dev/null 2>&1 || true
  if changed modula-firewall.service || ! systemctl is-active --quiet modula-firewall.service; then
    systemctl restart modula-firewall.service \
      || TIPS+=("Couldn't block the cloud metadata service for containers (systemctl status modula-firewall).")
  fi
  return 0
}

units() {
  CHANGED=" "
  buildkit_apparmor
  release_key
  metadata_firewall_unit

  # The edge. With socket activation, systemd holds 80/443 while Caddy
  # restarts (connections wait in the backlog instead of being refused); the
  # order of the sockets is fixed: fd 3 = TCP 80, fd 4 = TCP 443, fd 5 = UDP 443.
  unit modula-caddy.socket <<EOF
[Unit]
Description=Modula Deploy edge ports (held open while Caddy restarts)

[Socket]
# Caddy's config names these fd/3, fd/4 and fdgram/5 (modula-node --caddy-sockets).
ListenStream=80
ListenStream=443
ListenDatagram=443
Backlog=4096

[Install]
WantedBy=sockets.target
EOF
  unit modula-caddy.service <<EOF
[Unit]
Description=Modula Deploy edge (Caddy, configured by modula-node)
StartLimitIntervalSec=0
After=network-online.target
Wants=network-online.target
Requires=modula-caddy.socket
After=modula-caddy.socket

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
# In-flight requests get Caddy's grace period (10 s) to finish.
TimeoutStopSec=20
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

  local args=" --caddy-sockets"
  if [[ $MODE == main || -f $CONF/server.env ]]; then
    args+=" --route $(server_env MODULA_PUBLIC_URL "" | sed 's|.*https://||')=$(server_env MODULA_LISTEN 127.0.0.1:7100)"
  fi
  unit modula-node.service <<EOF
[Unit]
Description=Modula Deploy node (runs this server's workloads)
StartLimitIntervalSec=0
After=network-online.target docker.service modula-caddy.service
Wants=network-online.target docker.service modula-caddy.service

[Service]
# Manages Docker, Caddy, ufw and iptables, which is root-equivalent anyway;
# the sandboxing below only takes away what it never needs.
EnvironmentFile=-$CONF/node.env
ExecStart=$PREFIX/current/modula-node --data-dir $NODE_STATE run --caddy-admin unix:$CADDY_SOCKET --buildctl $PREFIX/current/buildctl --railpack $PREFIX/current/railpack --wal-g $PREFIX/current/wal-g --workerd $PREFIX/current/workerd$args
Restart=always
RestartSec=2
KillMode=mixed
TimeoutStopSec=10
# Apps go first when memory runs out, not the node that restarts them.
OOMScoreAdjust=-500
NoNewPrivileges=yes
PrivateTmp=yes
ProtectHome=yes
SystemCallArchitectures=native

[Install]
WantedBy=multi-user.target
EOF

  if [[ -f $CONF/server.env ]]; then
    unit modula-server.socket <<EOF
[Unit]
Description=Modula Deploy control plane ports (held open while it restarts)

[Socket]
# The API behind Caddy (MODULA_LISTEN) and the node endpoint (MODULA_NODE_LISTEN).
ListenStream=$(server_env MODULA_LISTEN 127.0.0.1:7100)
ListenStream=$(server_env MODULA_NODE_LISTEN 127.0.0.1:7101)
Backlog=4096

[Install]
WantedBy=sockets.target
EOF
    unit modula-server.service <<EOF
[Unit]
Description=Modula Deploy control plane
StartLimitIntervalSec=0
After=network-online.target postgresql.service redis-server.service
Wants=network-online.target postgresql.service redis-server.service
Requires=modula-server.socket
After=modula-server.socket

[Service]
User=modula
Group=modula
EnvironmentFile=$CONF/server.env
ExecStart=$PREFIX/current/modula-server serve
Restart=always
RestartSec=2
# In-flight requests get up to 20 s to finish (streams end right away).
TimeoutStopSec=30
OOMScoreAdjust=-500
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
ReadWritePaths=$STATE $UPGRADE_DIR

[Install]
WantedBy=multi-user.target
EOF
  fi

  upgrade_unit
  [[ $CHANGED != " " ]] && systemctl daemon-reload
  systemctl enable --now modula-upgrade.path >/dev/null 2>&1
  metadata_firewall_start
  return 0
}

# Upgrades requested by modula-server (the dashboard: "Update now", automatic
# updates) or by modula-node (rolling upgrades from the control plane). Neither
# runs as anything that may upgrade the server: they write the version to
# $UPGRADE_DIR/request, and this root-owned path unit runs the installer's
# upgrade for it (signature checks, health check, rollback). All a requester
# picks is the version, which must be a release no older than what runs; the
# installer comes from where this server was installed from.
upgrade_unit() {
  if [[ -f $CONF/server.env ]]; then
    install -d -m 0770 -o root -g modula "$UPGRADE_DIR"
  else
    install -d -m 0700 "$UPGRADE_DIR"
  fi
  # Joined servers get the installer (and so the version) from their
  # instance; a later upgrade with the public installer keeps that.
  local url=$INSTALL_URL releases=$SERVER_RELEASES
  if [[ ! -f $CONF/server.env && -z $SERVER_RELEASES && -f $CONF/upgrade.env ]]; then
    url=$(sed -n 's/^INSTALL_URL=//p' "$CONF/upgrade.env" | tail -1)
    releases=$(sed -n 's/^MODULA_SERVER_RELEASES=//p' "$CONF/upgrade.env" | tail -1)
  fi
  [[ -f $CONF/server.env ]] && releases=""
  # Never the release key: upgrade.sh only trusts $RELEASE_KEY_FILE.
  {
    echo "# Written by the Modula Deploy installer; read by $PREFIX/upgrade.sh."
    echo "INSTALL_URL=$url"
    if [[ -n $releases ]]; then echo "MODULA_SERVER_RELEASES=$releases"; fi
    if [[ -n ${MODULA_RELEASE_URL:-} ]]; then echo "MODULA_RELEASE_URL=$MODULA_RELEASE_URL"; fi
  } >"$CONF/upgrade.env"
  chmod 0644 "$CONF/upgrade.env"

  cat >"$PREFIX/upgrade.sh" <<'EOF'
#!/usr/bin/env bash
# Runs one upgrade that modula-server or modula-node asked for
# (modula-upgrade.path → modula-upgrade.service), then reports the result in
# /var/lib/modula-upgrade/result for the requester to read.
#
# The installer that performs it comes from the requested release and runs
# only once its signature checks out against /etc/modula/release.pub.pem:
# SHA256SUMS signed by the release key, install.sh's hash listed in it.
set -uo pipefail
dir=/var/lib/modula-upgrade
req=$dir/request
[[ -f $req && ! -L $req ]] || exit 0
version=$(head -c 64 "$req" | tr -d '[:space:]')
rm -f "$req" # first: the path unit fires again while it exists
from=$(basename "$(readlink -f /opt/modula/current)")
log=$(mktemp)
work=""
report() { # report STATUS — written atomically: the requester can write to $dir
  local tmp; tmp=$(mktemp "$dir/.result.XXXXXX")
  {
    echo "status=$1"
    echo "from=$from"
    echo "to=$version"
    echo "finished_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo
    tail -c 16000 "$log"
  } >"$tmp"
  chgrp --reference="$dir" "$tmp" 2>/dev/null || true
  chmod 0640 "$tmp"
  mv -fT "$tmp" "$dir/result"
  rm -f "$log"
  [[ -z $work ]] || rm -rf "$work"
}
fail() { echo "$1" >>"$log"; report failed; exit 1; }
if [[ ! $version =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]+)?$ ]]; then
  fail "not a release version: $version"
fi
if [[ $(printf '%s\n%s\n' "$from" "$version" | sort -V | tail -1) != "$version" ]]; then
  fail "refusing to downgrade from $from to $version"
fi
# Read, never sourced: only these three settings, and never a key.
setting() { sed -n "s/^$1=//p" /etc/modula/upgrade.env 2>/dev/null | tail -1; }
install_url=$(setting INSTALL_URL)
server_releases=$(setting MODULA_SERVER_RELEASES)
release_url=$(setting MODULA_RELEASE_URL)
pubkey=/etc/modula/release.pub.pem
[[ -s $pubkey ]] || fail "no release key at $pubkey; upgrade by hand: curl -fsSL ${install_url:-https://usemodula.org/install.sh} | sudo bash -s -- upgrade"
work=$(mktemp -d)
# verified SOURCE — fetches install.sh, SHA256SUMS and its signature from one
# place (the instance's release files, or the public release) and checks them.
verified() {
  local f
  rm -f "$work"/*
  for f in SHA256SUMS SHA256SUMS.sig install.sh; do
    curl -fsSL --retry 3 -o "$work/$f" "$1$f" 2>>"$log" || return 1
  done
  openssl pkeyutl -verify -pubin -inkey "$pubkey" -rawin \
    -in "$work/SHA256SUMS" -sigfile "$work/SHA256SUMS.sig" >/dev/null 2>>"$log" \
    || { echo "SHA256SUMS from $1 is not signed by the release key" >>"$log"; return 1; }
  grep -qE '^[0-9a-f]{64}  install\.sh$' "$work/SHA256SUMS" \
    || { echo "install.sh is not in SHA256SUMS from $1" >>"$log"; return 1; }
  (cd "$work" && grep -E '^[0-9a-f]{64}  install\.sh$' SHA256SUMS | sha256sum --check --quiet --strict -) >>"$log" 2>&1 \
    || { echo "install.sh from $1 does not match SHA256SUMS" >>"$log"; return 1; }
}
ok=0
if [[ -n $server_releases ]] && verified "$server_releases"; then ok=1; fi
if [[ $ok == 0 ]] && verified "${release_url:-https://downloads.usemodula.org/releases}/download/v$version/"; then ok=1; fi
[[ $ok == 1 ]] || fail "couldn't get a verified installer for $version"
rc=0
# (An empty MODULA_RELEASE_URL means the default.)
env NO_COLOR=1 MODULA_INSTALL_URL="$install_url" MODULA_SERVER_RELEASES="$server_releases" \
  MODULA_RELEASE_URL="$release_url" bash "$work/install.sh" upgrade --version "$version" >>"$log" 2>&1 || rc=$?
case $rc in
  0) report succeeded ;;
  3) report rolled_back ;; # the new version didn't start; the previous one runs again
  *) report failed ;;
esac
exit "$rc"
EOF
  chmod 0755 "$PREFIX/upgrade.sh"

  unit modula-upgrade.service <<EOF
[Unit]
Description=Modula Deploy upgrade (requested by modula-server or modula-node)

[Service]
Type=oneshot
ExecStart=$PREFIX/upgrade.sh
TimeoutStartSec=20min
EOF
  unit modula-upgrade.path <<EOF
[Unit]
Description=Modula Deploy upgrade requests

[Path]
PathExists=$UPGRADE_DIR/request

[Install]
WantedBy=paths.target
EOF
}

# Waits for the API to answer. A socket-activated port accepts connections
# even while nothing serves them, so every attempt is bounded.
wait_healthy() {
  local deadline=$((SECONDS + 60))
  while ((SECONDS < deadline)); do
    curl -fsS --max-time 2 -o /dev/null http://127.0.0.1:7100/api/health 2>/dev/null && return 0
    sleep 0.2
  done
  journalctl -u modula-server -n 30 --no-pager >&2 || true
  echo "modula-server did not become healthy" >&2
  return 1
}

# enable_units NAME… — enables and starts each service and its socket unit.
enable_units() {
  local n
  for n in "$@"; do
    # Only some units have a socket (errexit is on: a missing one would end the step).
    if [[ -f /etc/systemd/system/$n.socket ]]; then systemctl enable --now "$n.socket" >/dev/null 2>&1; fi
    systemctl enable --now "$n.service" >/dev/null 2>&1
  done
  return 0
}

start_main() {
  enable_units modula-caddy modula-server
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
  server=$(sed -nE 's/.*--server ([^ ]+).*/\1/p' <<<"$cmd")
  # The token as `--token mcj_…` or `MODULA_JOIN_TOKEN=mcj_…`.
  token=$(grep -oE '(--token |MODULA_JOIN_TOKEN=)'"'"'?mcj_[A-Za-z0-9_-]+' <<<"$cmd" | grep -oE 'mcj_[A-Za-z0-9_-]+' | head -1 || true)
  ca=$(sed -nE 's/.*--ca ([^ ]+).*/\1/p' <<<"$cmd")
  [[ -n $server && -n $token && -n $ca ]] || { echo "unexpected output from modula-server join-token" >&2; return 1; }
  # This server dials itself over loopback; the certificate covers 127.0.0.1.
  # The token goes in the environment, not on modula-node's command line.
  MODULA_JOIN_TOKEN=$token "$PREFIX/current/modula-node" --data-dir "$NODE_STATE" join \
    --server "https://127.0.0.1:${server##*:}" --ca "$ca" --name "${NODE_NAME:-$(hostname -s)}"
}

join_remote() {
  [[ -n $(ls -A "$NODE_STATE" 2>/dev/null) ]] && return 0 # already joined
  local args=(--server "$JOIN_SERVER" --ca "$JOIN_CA")
  [[ -n $NODE_NAME ]] && args+=(--name "$NODE_NAME")
  # Private mode: modula-node sets up `cloudflared access tcp` before joining.
  [[ -n ${ACCESS_HOST:-} ]] && args+=(--access-host "$ACCESS_HOST" --access-id "$ACCESS_ID")
  # Secrets via the environment, never modula-node's command line.
  MODULA_JOIN_TOKEN=$JOIN_TOKEN MODULA_ACCESS_SECRET=${ACCESS_SECRET:-} \
    "$PREFIX/current/modula-node" --data-dir "$NODE_STATE" join "${args[@]}"
}

start_node() {
  enable_units modula-caddy modula-node
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
short_host() { local h; h=$(hostname -s 2>/dev/null || uname -n); printf '%s' "${h%%.*}"; }

# ask VAR "question" "default" check "hint" — reads one answer from PROMPT_FD
# until check accepts it. VAR is always a literal name from this script.
ask() {
  local ans
  while :; do
    printf '  %s?%s %s%s%s ' "$A" "$N" "$B" "$2" "$N"
    [[ -n $3 ]] && printf '%s[%s]%s ' "$D" "$3" "$N"
    IFS= read -r -u "$PROMPT_FD" ans || { printf '\n'; die "no answer, so nothing was installed" "Run it again when you're ready."; }
    ans=${ans#"${ans%%[![:space:]]*}"}; ans=${ans%"${ans##*[![:space:]]}"}
    [[ -z $ans ]] && ans=$3
    [[ -z $ans && $4 == valid_yes_no ]] && ans=y
    if [[ -n $ans ]] && "$4" "$ans"; then
      printf -v "$1" '%s' "$ans"
      return 0
    fi
    printf '    %s%s%s\n' "$R" "$5" "$N"
  done
}

# This server's public IPv4 address, or nothing.
public_ip() {
  local ip
  ip=$(curl -fsS --max-time 5 https://1.1.1.1/cdn-cgi/trace 2>/dev/null | sed -n 's/^ip=//p' || true)
  [[ $ip =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]] && printf '%s' "$ip"
  return 0
}

# Warns (never fails) when the domain doesn't point at this server yet.
check_dns() {
  local ip=$1 addrs
  [[ -n $ip ]] || return 0
  addrs=$(getent ahostsv4 "$DOMAIN" 2>/dev/null | awk '{print $1}' | sort -u | tr '\n' ' ' || true)
  addrs=${addrs% }
  if [[ -z $addrs ]]; then
    printf '    %s! %s doesn'"'"'t resolve yet. Fine if you just added the record; the%s\n' "$D" "$DOMAIN" "$N"
    printf '    %s  certificate is issued once it points at %s.%s\n' "$D" "$ip" "$N"
  elif [[ " $addrs " != *" $ip "* ]]; then
    printf '    %s! %s points at %s, not this server (%s).%s\n' "$D" "$DOMAIN" "$addrs" "$ip" "$N"
    printf '    %s  Fine if you just changed it; the certificate waits for DNS.%s\n' "$D" "$N"
  else
    printf '    %s✓ %s points here.%s\n' "$D" "$DOMAIN" "$N"
  fi
}

questions() {
  if [[ -z $EMAIL ]]; then
    note "An email for TLS certificates (Let's Encrypt) and your admin account."
    ask EMAIL "Email" "" valid_email "That doesn't look like an email address, e.g. you@example.com."
    printf '\n'
  fi
  if [[ -z $NODE_NAME ]]; then
    local def; def=$(short_host)
    valid_name "$def" || def=main
    note "A name for this server on the dashboard's Servers page."
    ask NODE_NAME "Server name" "$def" valid_name "Letters, digits, . _ - only, up to 63 characters, e.g. web-1."
    printf '\n'
  fi
  if [[ $HARDEN_ASKED == 0 ]]; then
    local harden
    note "A firewall (SSH, 80, 443 and Modula's ports), security updates, log caps, swap if small."
    ask harden "Set up the firewall and basic hardening? ${D}[Y/n]${N}" "" valid_yes_no "Please answer y or n."
    [[ ${harden,,} == y* ]] || { HARDEN=0; HARDEN_WHY="as you asked"; }
    printf '\n'
  fi
  local yes
  ask yes "Install Modula Deploy on $(this_host) as $NODE_NAME for $DOMAIN? ${D}[Y/n]${N}" "" valid_yes_no "Please answer y or n."
  if [[ ${yes,,} != y* ]]; then
    note "Okay, nothing was installed. Run it again whenever you like."
    printf '\n'
    exit 0
  fi
  printf '\n'
}
# Enter alone means yes.
valid_yes_no() { [[ ${1,,} =~ ^(y|yes|n|no)$ ]]; }

install_main() {
  header "Hey! Let's get Modula running on $(this_host). This takes a few minutes."
  if [[ $PROMPT == 1 ]]; then
    questions
    exec 5<&- # the terminal isn't needed past this point
  fi
  valid_ipv4 "$DOMAIN" || check_dns "$(public_ip)"
  if [[ -n ${MODULA_INSTALL_DRY_RUN:-} ]]; then
    card_top "Dry run: nothing was changed"
    card_kv "Running as" "$(id -un)"
    card_kv "Domain" "$DOMAIN"
    card_kv "Email" "$EMAIL"
    card_kv "Server name" "${NODE_NAME:-$(short_host)}"
    card_kv "Apps domain" "${APPS_DOMAIN:-none}"
    card_kv "Version" "$VERSION"
    card_kv "Host setup" "$(dry_harden)"
    card_end "Bye!"
    exit 0
  fi
  progress_start
  step "Checking this server" preflight
  step "Installing packages" packages
  harden
  step "Downloading release ($VERSION)" fetch_release
  step "Creating users and directories" accounts
  step "Setting up Postgres" database
  step "Setting up Redis" redis
  step "Writing configuration" server_config
  step "Creating the master key" master_key
  # Caddy takes port 80 from here on; the CLI follows the dashboard instead.
  PROGRESS_STATE=handoff; progress_write
  progress_stop
  step "Writing services" units
  step "Starting the control plane" start_main
  step "Adding this server as a node" join_local
  step "Starting the node and edge" start_node
  step "Writing the recovery kit" recovery_kit
  local_only
  ssh_tip
  # A preseeded token (--setup-token) stays the one in effect: minting a
  # new one here would replace it.
  local setup=""
  [[ -n $SETUP_TOKEN_HASH ]] || setup=$(server_cli setup-token 2>/dev/null || true)
  card_top "All set! Modula is up and running  ${D}($(fmt_ms "$(ms_since "$T0")"))${N}"
  card ""
  card_kv "Dashboard" "${A}https://$DOMAIN${N}"
  if [[ -n $setup ]]; then
    card_kv "Setup token" "${B}$setup${N}"
    card_kv "" "${D}single use; create the first admin with it${N}"
  elif [[ -n $SETUP_TOKEN_HASH ]]; then
    card_kv "Setup token" "the one passed with --setup-token"
    card_kv "" "${D}modula server create finishes setup with it${N}"
  fi
  card_kv "Recovery kit" "/root/modula-recovery-kit.txt"
  card_kv "" "${R}copy it somewhere safe, then delete it from this server${N}"
  card ""
  card "${B}Next:${N} open ${A}https://$DOMAIN/setup${N}${setup:+ and paste the setup token}."
  card "${D}Create your admin account there, e.g. as $EMAIL.${N}"
  security_card
  if valid_ipv4 "$DOMAIN"; then
    card "${D}Add a domain in the dashboard (or: modula instance domain set). A cloud${N}"
  else
    card "${D}Point DNS for $DOMAIN${APPS_DOMAIN:+ and *.$APPS_DOMAIN} at this server. A cloud${N}"
  fi
  card "${D}firewall in front of it needs 80, 443 (tcp+udp) and $NODE_PORT open.${N}"
  card ""
  card_end "Have fun shipping things!"
  print_tips
}

# The Security line of the closing card.
security_card() {
  [[ ${#SECURITY[@]} -eq 0 ]] && return 0
  local first=1 s
  for s in "${SECURITY[@]}"; do
    if [[ $first == 1 ]]; then card_kv "Security" "$s"; first=0; else card_kv "" "$s"; fi
  done
  card ""
}

dry_harden() {
  if [[ $HARDEN == 0 ]]; then echo "skipped"
  elif [[ $FIREWALL == 0 ]]; then echo "updates, logs, swap, clock, kernel (no firewall)"
  else echo "firewall ($(firewall_rules | sed 's/:/-/' | paste -sd' ')), updates, logs, swap, clock, kernel"
  fi
}

install_join() {
  header "Joining $JOIN_SERVER as a new node…"
  step "Checking this server" preflight
  step "Installing packages" packages
  harden
  step "Downloading release ($VERSION)" fetch_release
  step "Creating users and directories" accounts
  step "Writing services" units
  step "Joining" join_remote
  step "Starting the node and edge" start_node
  ssh_tip
  card_top "Welcome aboard!  ${D}($(fmt_ms "$(ms_since "$T0")"))${N}"
  card ""
  card "$(this_host) is now part of ${A}$JOIN_SERVER${N}."
  card "It'll show up on the Servers page in a moment, ready for apps."
  card ""
  security_card
  card "${D}Apps need 80 and 443 (tcp+udp) open; the dashboard needs no inbound port.${N}"
  card ""
  card_end "Happy shipping!"
  print_tips
}

# ---------------------------------------------------------------- main --restore
# Moving the control plane here (see the top of this file). modula-node runs
# this as root with its output in a log whose tail goes back to the old
# control plane, so nothing may ask, and every failure says what broke.

# The bundle's members: these exact names (an optional ./ in front), plain
# files and directories only. Anything else (links, devices, absolute paths,
# .., duplicates) means it isn't a bundle the old control plane wrote.
BUNDLE_MEMBER='^(\./)?(promotion\.env|control-plane\.dump|master\.key|uploads/?|uploads/.+|)$'
PROMO_ENV=""

# Checks the bundle without unpacking it and reads its promotion.env.
restore_env() {
  [[ -f $RESTORE && ! -L $RESTORE ]] || die "no promotion bundle at $RESTORE" "The old control plane sends it; start the migration again from the dashboard."
  [[ -r $RESTORE ]] || die "can't read $RESTORE" "Run this as root."
  local names types plain n env_member="" bad
  names=$(LC_ALL=C tar -tf "$RESTORE" 2>&1) || die "$RESTORE is not a readable tar archive" "The bundle may have been cut short; start the migration again."
  types=$(LC_ALL=C tar -tvf "$RESTORE" 2>&1 | cut -c1) || die "$RESTORE is not a readable tar archive" "The bundle may have been cut short; start the migration again."
  while IFS= read -r n; do
    [[ $n =~ $BUNDLE_MEMBER && ! $n =~ (^|/)\.\.(/|$) && $n != /* ]] \
      || die "the promotion bundle holds an unexpected member: $n" "Only promotion.env, control-plane.dump, master.key and uploads/ belong in it."
    [[ ${n#./} == promotion.env ]] && env_member=$n
  done <<<"$names"
  bad=$(tr -d -- '-d\n' <<<"$types")
  [[ -z $bad ]] || die "the promotion bundle holds links or special files" "Only plain files and directories belong in it."
  plain=$(sed 's|^\./||; s|/$||' <<<"$names")
  [[ -z $(sort <<<"$plain" | uniq -d) ]] || die "the promotion bundle lists a member twice"
  for n in promotion.env control-plane.dump master.key; do
    grep -qx "$n" <<<"$plain" || die "the promotion bundle has no $n" "The bundle may have been cut short; start the migration again."
  done
  # Read, never sourced; only the keys below, each checked.
  PROMO_ENV=$(tar -xOf "$RESTORE" "$env_member" | tr -d '\r') || die "can't read promotion.env from $RESTORE"
  ((${#PROMO_ENV} <= 65536)) || die "the bundle's promotion.env is too big to be one"
  promo() { sed -n "s/^$1=//p" <<<"$PROMO_ENV" | tail -1; }
  local url host ip port
  MIGRATION_ID=$(promo MIGRATION_ID)
  # A Modula id, as modula-server parses MODULA_PROMOTED_MIGRATION (else it
  # would refuse to start after the restore).
  [[ $MIGRATION_ID =~ ^[0-9a-z]{16}$ ]] || die "promotion.env: MIGRATION_ID is missing or malformed"
  url=$(promo PUBLIC_URL)
  host=${url#https://}; host=${host,,}
  { [[ $url == https://* ]] && valid_dashboard "$host"; } || die "promotion.env: PUBLIC_URL must be https://HOST (a hostname or IPv4, no port or path), not '$url'"
  DOMAIN=$host
  NODE_URL=$(promo NODE_URL)
  # (valid_ipv4 overwrites BASH_REMATCH.)
  ip="" port=0
  [[ $NODE_URL =~ ^https://([0-9.]+):([1-9][0-9]{0,4})$ ]] && ip=${BASH_REMATCH[1]} port=${BASH_REMATCH[2]}
  { [[ -n $ip ]] && ((port <= 65535)) && valid_ipv4 "$ip"; } || die "promotion.env: NODE_URL must be https://IPv4:PORT, not '$NODE_URL'"
  NODE_PORT=$(promo NODE_PORT)
  { [[ $NODE_PORT =~ ^[1-9][0-9]{0,4}$ ]] && ((NODE_PORT <= 65535)); } || die "promotion.env: NODE_PORT must be a port number, not '$NODE_PORT'"
  EMAIL=$(promo ACME_EMAIL)
  [[ -z $EMAIL ]] || valid_email "$EMAIL" || die "promotion.env: ACME_EMAIL is not an email address: '$EMAIL'"
  APPS_DOMAIN=$(promo APPS_DOMAIN); APPS_DOMAIN=${APPS_DOMAIN,,}
  [[ -z $APPS_DOMAIN ]] || valid_host "$APPS_DOMAIN" || die "promotion.env: APPS_DOMAIN is not a hostname: '$APPS_DOMAIN'"
  TCP_PROXY_HOST=$(promo TCP_PROXY_HOST); TCP_PROXY_HOST=${TCP_PROXY_HOST,,}
  [[ -z $TCP_PROXY_HOST ]] || valid_dashboard "$TCP_PROXY_HOST" || die "promotion.env: TCP_PROXY_HOST is not a hostname or IPv4: '$TCP_PROXY_HOST'"
  OLD_VERSION=$(promo VERSION)
  [[ $OLD_VERSION =~ ^[0-9A-Za-z.+-]{0,64}$ ]] || OLD_VERSION="?"
  return 0
}

# Dev installs fetch releases from elsewhere; upgrade.env (rewritten by
# units) keeps saying where.
restore_release_url() {
  [[ -z ${MODULA_RELEASE_URL:-} && -f $CONF/upgrade.env ]] || return 0
  local u; u=$(sed -n 's/^MODULA_RELEASE_URL=//p' "$CONF/upgrade.env" | tail -1)
  [[ -z $u ]] || { MODULA_RELEASE_URL=$u; RELEASE_URL=$u; }
}

# (From preflight.) Only a joined server becomes the main server, and only
# once: a server.env is either this migration's (a re-run resumes it) or a
# control plane of its own.
restore_preflight() {
  [[ -f $NODE_STATE/node.json ]] || die "this server hasn't joined the instance ($NODE_STATE/node.json is missing)" "Only a server that already runs as a node can become the main server."
  [[ -d $PREFIX/current/ ]] || die "no Modula release at $PREFIX/current" "Only a server that already runs as a node can become the main server."
  if [[ -f $CONF/server.env && $(server_env MODULA_PROMOTED_MIGRATION "") != "$MIGRATION_ID" ]]; then
    die "this server already runs a control plane" "Its $CONF/server.env isn't from migration $MIGRATION_ID; pick a server that only runs as a node."
  fi
  return 0
}

# Worker releases leave out modula-server: fetch just that binary into the
# release that runs here, checked against the release's own SHA256SUMS once
# its signature checks out (download() would keep the release dir as it is).
restore_server_binary() {
  local dir n=modula-server-linux-$ARCH key=$RELEASE_KEY_FILE tmpkey="" sum tmp
  dir=$(readlink -f "$PREFIX/current")
  if [[ ! -s $key ]]; then
    tmpkey=$(mktemp); key=$tmpkey
    printf -- "-----BEGIN PUBLIC KEY-----\n%s\n-----END PUBLIC KEY-----\n" "$RELEASE_PUBKEY" >"$key"
  fi
  if ! openssl pkeyutl -verify -pubin -inkey "$key" -rawin -in "$dir/SHA256SUMS" -sigfile "$dir/SHA256SUMS.sig" >/dev/null; then
    [[ -z $tmpkey ]] || rm -f "$tmpkey"
    echo "$dir/SHA256SUMS is not signed by the release key; refusing to install" >&2
    return 1
  fi
  [[ -z $tmpkey ]] || rm -f "$tmpkey"
  sum=$(awk -v n="$n" '$2 == n { print $1; exit }' "$dir/SHA256SUMS")
  [[ $sum =~ ^[0-9a-f]{64}$ ]] || { echo "$n is not in $dir/SHA256SUMS" >&2; return 1; }
  matches() { [[ -f $1 && $(sha256sum "$1" | awk '{print $1}') == "$sum" ]]; }
  if ! matches "$dir/$n"; then
    # The instance this server joined serves the same release, close by.
    local SERVER_RELEASES=""
    [[ -f $CONF/upgrade.env ]] && SERVER_RELEASES=$(sed -n 's/^MODULA_SERVER_RELEASES=//p' "$CONF/upgrade.env" | tail -1)
    tmp=$(mktemp "$dir/.$n.XXXXXX")
    artifact "$n" "$tmp" "$dir/SHA256SUMS" || { rm -f "$tmp"; return 1; }
    matches "$tmp" || { rm -f "$tmp"; echo "$n: checksum mismatch; refusing to install" >&2; return 1; }
    chmod 755 "$tmp"
    mv -f "$tmp" "$dir/$n"
  fi
  ln -sfn "$n" "$dir/modula-server"
  ln -sfn "$PREFIX/current/modula-server" /usr/local/bin/modula-server
}

# Unpacks the bundle into a fresh private directory next to where its
# contents go (one filesystem: moving them in is a rename).
restore_unpack() {
  rm -rf "$STATE"/.promotion.* # left by a run that was killed outright
  RESTORE_TMP=$(mktemp -d "$STATE/.promotion.XXXXXX")
  chmod 0700 "$RESTORE_TMP"
  tar -xf "$RESTORE" -C "$RESTORE_TMP" --no-same-owner --no-same-permissions
  [[ -f $RESTORE_TMP/control-plane.dump && -f $RESTORE_TMP/master.key && ! -L $RESTORE_TMP/master.key ]] \
    || { echo "the bundle didn't unpack as expected" >&2; return 1; }
}

# The instance's own key: every secret in the database is encrypted with it.
# An existing, different key is never replaced.
restore_master_key() {
  local src=$RESTORE_TMP/master.key dst=$STATE/master.key
  [[ -s $src ]] || die "the bundle's master.key is empty"
  if [[ -e $dst ]]; then
    cmp -s "$src" "$dst" || die "$dst already exists and differs from the bundle's master key" "It belongs to another instance; this server can't take over this one without removing it by hand."
  else
    install -m 0600 -o modula -g modula "$src" "$dst.new"
    mv -f "$dst.new" "$dst"
  fi
  chown modula:modula "$dst"
  chmod 0600 "$dst"
}

# Source uploads that deployments may still build from. Replaces what an
# earlier attempt moved in (it came from the same bundle).
restore_uploads() {
  [[ -d $RESTORE_TMP/uploads ]] || return 0
  chown -R modula:modula "$RESTORE_TMP/uploads"
  chmod -R u+rwX,go-rwx "$RESTORE_TMP/uploads"
  rm -rf "$STATE/uploads.old"
  [[ -e $STATE/uploads ]] && mv "$STATE/uploads" "$STATE/uploads.old"
  mv "$RESTORE_TMP/uploads" "$STATE/uploads"
  rm -rf "$STATE/uploads.old"
}

# The old control plane's database, as modula (peer authentication, like
# modula-server). --clean --if-exists makes a re-run start over; one
# transaction leaves the database as it was if anything fails.
restore_database() {
  local dump=$STATE/.promotion.dump rc=0
  # An earlier attempt's modula-server would hold locks the restore waits on
  # (and its socket would start it again on the next connection).
  systemctl stop modula-server.socket modula-server.service 2>/dev/null || true
  mv -f "$RESTORE_TMP/control-plane.dump" "$dump"
  chown modula:modula "$dump"
  chmod 0600 "$dump"
  runuser -u modula -- pg_restore --clean --if-exists --no-owner --no-privileges --single-transaction --dbname=modula "$dump" || rc=$?
  rm -f "$dump"
  [[ $rc == 0 ]] || die "pg_restore couldn't restore the control plane's database (exit $rc)" \
    "Its errors are above. A dump from a newer Postgres than this server's ($(pg_restore --version 2>/dev/null | awk '{print $3}')) can't be restored here."
}

# The other servers dial the control plane at NODE_URL now.
restore_firewall() {
  [[ $FIREWALL == 1 ]] || return 0
  if ufw_active; then
    ufw allow "$NODE_PORT/tcp" comment modula
    return 0
  fi
  local other; other=$(other_firewall)
  [[ -z $other ]] || TIPS+=("Firewall: $other is managing this server; allow $NODE_PORT/tcp so the other servers can reach the control plane.")
  return 0
}

# Done: the bundle (it holds the master key) goes; the recovery kit has it.
restore_cleanup() {
  [[ -z $RESTORE_TMP ]] || { [[ ! -f $RESTORE_TMP/master.key ]] || shred -u "$RESTORE_TMP/master.key"; rm -rf "$RESTORE_TMP"; }
  RESTORE_TMP=""
  shred -n 1 -u "$RESTORE" 2>/dev/null || rm -f "$RESTORE"
}

install_restore() {
  header "Making $(this_host) the main server…"
  restore_env
  restore_release_url
  if [[ -n ${MODULA_INSTALL_DRY_RUN:-} ]]; then
    card_top "Dry run: nothing was changed"
    card_kv "Running as" "$(id -un)"
    card_kv "Migration" "$MIGRATION_ID"
    card_kv "Dashboard" "https://$DOMAIN"
    card_kv "Node URL" "$NODE_URL (port $NODE_PORT)"
    card_kv "Email" "${EMAIL:-none}"
    card_kv "Apps domain" "${APPS_DOMAIN:-none}"
    card_kv "TCP proxies" "${TCP_PROXY_HOST:-none}"
    card_kv "From version" "${OLD_VERSION:-?}"
    card_kv "Bundle" "$RESTORE"
    card_end "Bye!"
    exit 0
  fi
  step "Checking this server" preflight
  step "Installing packages" packages
  step "Fetching modula-server" restore_server_binary
  step "Creating users and directories" accounts
  step "Setting up Postgres" database
  step "Setting up Redis" redis
  step "Unpacking the bundle" restore_unpack
  step "Writing configuration" server_config
  step "Restoring the master key" restore_master_key
  step "Restoring uploads" restore_uploads
  step "Restoring the database" restore_database
  step "Opening the node port ($NODE_PORT)" restore_firewall
  # modula-node keeps running as it is: it still talks to the old control
  # plane, and restarts into the new unit (with the dashboard's route) when
  # that hands over.
  step "Writing services" units
  step "Starting the control plane" start_main
  step "Writing the recovery kit" recovery_kit
  restore_cleanup
  card_top "This server now runs the control plane; the old one hands over next.  ${D}($(fmt_ms "$(ms_since "$T0")"))${N}"
  card_kv "Dashboard" "https://$DOMAIN"
  card_kv "Migration" "$MIGRATION_ID"
  card_kv "Recovery kit" "/root/modula-recovery-kit.txt"
  card_end "Almost there."
  print_tips
}

# The checksum of a release's Caddy binary (empty when it has none).
caddy_sum() { sha256sum "$1/caddy" 2>/dev/null | awk '{print $1}'; }

upgrade() {
  [[ -L $PREFIX/current ]] || die "Modula Deploy is not installed here" "Install it first: curl -fsSL https://usemodula.org/install.sh | bash"
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
  local caddy_before; caddy_before=$(caddy_sum "$previous")
  step "Switching to $(basename "$RELEASE")" activate
  step "Writing services" units
  if [[ $MODE == main ]]; then
    # Socket-activated, the API's port stays open: requests wait for the new
    # process while the old one finishes what it already accepted.
    systemctl restart modula-server
    if ! wait_healthy; then
      printf '  %s✗ The new version did not start; rolling back to %s.%s\n' "$R" "$(basename "$previous")" "$N" >&2
      RELEASE=$previous activate
      units
      systemctl restart modula-server
      systemctl start modula-caddy modula-node # (no-ops when running)
      wait_healthy || true
      printf '    %s\n' "Your apps keep running. The log lines above should say what went wrong." >&2
      exit 3 # modula-upgrade reports this as rolled back
    fi
  fi
  # Caddy only restarts when it changed: its config lives in the running
  # process (the node restores it after a restart), and app traffic never
  # notices a node restart.
  step "Restarting the node" systemctl restart modula-node
  if [[ $(caddy_sum "$RELEASE") != "$caddy_before" ]] || changed modula-caddy.service || changed modula-caddy.socket; then
    step "Restarting the edge" systemctl restart modula-caddy
  else
    skipped "Restarting the edge" "Caddy is unchanged"
  fi
  systemctl start modula-caddy # (a no-op when running)
  prune_releases
  printf '\n  %s✓%s Upgraded to %s%s%s in %s. Fresh and ready!\n\n' "$A" "$N" "$B" "$(basename "$RELEASE")" "$N" "$(fmt_ms "$(ms_since "$T0")")"
}

main() {
  # A server installer: refuse other systems before anything else runs
  # (macOS's bash 3.2 can't even run the rest of this script).
  if [[ $(uname -s) != Linux ]]; then
    printf '\n  ✗ This installs a Modula server, on Ubuntu 22.04+ or Debian 12+.\n' >&2
    printf '    Run it on your server (over SSH). For the modula CLI on this machine:\n' >&2
    printf '      curl -fsSL https://usemodula.org/install-cli.sh | bash\n\n' >&2
    exit 1
  fi
  if ((BASH_VERSINFO[0] < 4)); then
    printf '\n  ✗ This installer needs bash 4 or newer (this is bash %s).\n\n' "$BASH_VERSION" >&2
    exit 1
  fi
  exec 3>&1 4>&2
  trap on_exit EXIT
  parse_args "$@"
  validate_args
  install_url
  elevate
  case $MODE in
    main) if [[ -n $RESTORE ]]; then install_restore; else install_main; fi ;;
    join) install_join ;;
    upgrade) upgrade ;;
  esac
}

main "$@"
