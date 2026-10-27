# Shared by the e2e scripts (sourced, from the repository root): the local
# release in its published layout, and privileged systemd containers on one
# Docker network standing in for fresh Ubuntu servers.
# shellcheck shell=bash

RELEASE=${RELEASE:-dist/release}
NET=${NET:-modula-e2e}
DOMAIN=${DOMAIN:-deploy.e2e.test}
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

build_image() {
  docker build -q -t modula-e2e-ubuntu -f deploy/e2e/ubuntu-systemd.Dockerfile deploy/e2e >/dev/null
  docker network create "$NET" >/dev/null 2>&1 || true
}

wait_systemd() { # wait_systemd NAME
  for _ in $(seq 1 100); do
    docker exec "$1" systemctl is-system-running 2>/dev/null | grep -qE "running|degraded" && return 0
    sleep 0.2
  done
  fail "$1: systemd did not start"
}

boot() { # boot NAME
  docker rm -f "$1" >/dev/null 2>&1 || true
  docker run -d --name "$1" --hostname "$1" --network "$NET" \
    --privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
    --tmpfs /run --tmpfs /run/lock \
    -v "$1-docker:/var/lib/docker" -v "$1-containerd:/var/lib/containerd" \
    -v "$RELEASE:/srv/releases/download/v$VERSION:ro" \
    -v "$FILES:/srv/releases/files:ro" \
    -v "$PWD/deploy/install.sh:/root/install.sh:ro" \
    modula-e2e-ubuntu >/dev/null
  wait_systemd "$1"
}

on() { docker exec "$@"; }
ip_of() { docker inspect -f "{{(index .NetworkSettings.Networks \"$NET\").IPAddress}}" "$1"; }
# shellcheck disable=SC2034 # used by the scripts that source this
env_vars=(-e MODULA_RELEASE_URL=file:///srv/releases -e MODULA_VERSION="$VERSION")
