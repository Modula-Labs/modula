#!/usr/bin/env bash
# Build a signed release: static Linux binaries + our Caddy build + signed sums.
#
#   scripts/release.sh                 # amd64 and arm64 into dist/release/
#   ARCHES=arm64 scripts/release.sh    # one architecture (faster; e2e uses this)
#
# Output (the layout install.sh downloads):
#   modula-server-linux-<arch>  modula-node-linux-<arch>  caddy-linux-<arch>
#   modula-deploy-linux-<arch> (the CLI; not installed on servers; install-cli.sh)
#   SHA256SUMS  SHA256SUMS.sig  (Ed25519 over SHA256SUMS)
#
# The signing key never enters a container or the repo. Default location:
# .dev/release-signing/release.key; its public half is deploy/release.pub.pem,
# which install.sh embeds.
set -euo pipefail
cd "$(dirname "$0")/.."

ARCHES=${ARCHES:-"amd64 arm64"}
OUT=${OUT:-dist/release}
KEY=${RELEASE_SIGNING_KEY:-.dev/release-signing/release.key}
CADDY_VERSION=${CADDY_VERSION:-v2.10.2}
XCADDY_VERSION=${XCADDY_VERSION:-v0.4.5}
# DNS-01 for wildcard certificates on generated domains.
CADDY_PLUGINS=(github.com/caddy-dns/cloudflare)

[[ -f $KEY ]] || { echo "signing key $KEY not found" >&2; exit 1; }
embedded=$(sed -n 's/^RELEASE_PUBKEY="\${MODULA_RELEASE_PUBKEY:-\(.*\)}"$/\1/p' deploy/install.sh)
actual=$(openssl pkey -in "$KEY" -pubout | sed -n 2p)
[[ $embedded == "$actual" ]] || { echo "install.sh embeds a different public key than $KEY" >&2; exit 1; }
cli_embedded=$(sed -n 's/^RELEASE_PUBKEY="\${MODULA_RELEASE_PUBKEY:-\(.*\)}"$/\1/p' scripts/install-cli.sh)
[[ $cli_embedded == "$actual" ]] || { echo "install-cli.sh embeds a different public key than $KEY" >&2; exit 1; }
cmp -s <(openssl pkey -in "$KEY" -pubout) deploy/release.pub.pem \
  || { echo "deploy/release.pub.pem does not match $KEY" >&2; exit 1; }

mkdir -p "$OUT"
rm -f "$OUT"/SHA256SUMS "$OUT"/SHA256SUMS.sig

echo "web: building the dashboard (embedded into modula-server)"
(cd web && pnpm install --frozen-lockfile >/dev/null && pnpm build >/dev/null)

for arch in $ARCHES; do
  echo "$arch: compiling modula-server, modula-node and the modula-deploy CLI"
  docker run --rm --platform "linux/$arch" \
    -v "$PWD":/src:ro \
    -v modula-cargo-registry:/usr/local/cargo/registry \
    -v "modula-release-target-$arch":/target \
    -v "$PWD/$OUT":/out \
    -e CARGO_TARGET_DIR=/target \
    -w /src rust:1-alpine \
    sh -euc "
      apk add -q --no-cache musl-dev protobuf-dev >/dev/null
      cargo build -q --release --locked -p modula-server -p modula-node -p modula-cli
      cp /target/release/modula-server /out/modula-server-linux-$arch
      cp /target/release/modula-node /out/modula-node-linux-$arch
      cp /target/release/modula-deploy /out/modula-deploy-linux-$arch
    "

  echo "$arch: building Caddy $CADDY_VERSION with ${CADDY_PLUGINS[*]}"
  with=()
  for p in "${CADDY_PLUGINS[@]}"; do with+=(--with "$p"); done
  docker run --rm \
    -v modula-go-cache:/root/go \
    -v "$PWD/$OUT":/out \
    -e CGO_ENABLED=0 -e GOOS=linux -e GOARCH="$arch" \
    golang:1.25-alpine \
    sh -euc "
      GOOS= GOARCH= go install github.com/caddyserver/xcaddy/cmd/xcaddy@$XCADDY_VERSION
      cd /tmp && xcaddy build $CADDY_VERSION ${with[*]} --output /out/caddy-linux-$arch
    "

  echo "$arch: fetching buildctl and railpack (pinned sums)"
  scripts/fetch-build-tools.sh "$arch" "$OUT"
done

(cd "$OUT" && sha256sum modula-*-linux-* caddy-linux-* buildctl-linux-* railpack-linux-* >SHA256SUMS)
openssl pkeyutl -sign -inkey "$KEY" -rawin -in "$OUT/SHA256SUMS" -out "$OUT/SHA256SUMS.sig"
openssl pkeyutl -verify -pubin -inkey deploy/release.pub.pem -rawin \
  -in "$OUT/SHA256SUMS" -sigfile "$OUT/SHA256SUMS.sig" >/dev/null
cp deploy/install.sh "$OUT/install.sh"
cp scripts/install-cli.sh "$OUT/install-cli.sh"
echo "release: $OUT ($(cd "$OUT" && ls | tr '\n' ' '))"
