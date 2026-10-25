#!/usr/bin/env bash
# Build a signed release: static Linux binaries + our Caddy build + signed sums.
#
#   scripts/release.sh                 # amd64 and arm64 into dist/release/
#   ARCHES=arm64 scripts/release.sh    # one architecture (faster; e2e uses this)
#   scripts/release.sh --check         # only check the signing key, build nothing
#
# Output (the layout install.sh downloads):
#   modula-server-linux-<arch>  modula-node-linux-<arch>  caddy-linux-<arch>
#   modula-linux-<arch>, modula-darwin-<arch> (the CLI; not installed on servers; install-cli.sh)
#   VERSION     (the release's version; instances check it daily for updates)
#   install.sh  install-cli.sh  (upgrades run install.sh only once it checks out)
#   LICENSE  THIRD_PARTY_NOTICES.md
#   SHA256SUMS  SHA256SUMS.sig  (Ed25519 over SHA256SUMS, which lists all of the above)
#
# The signing key never enters a container or the repo. Default location:
# ~/.config/modula/release-signing/release.key (RELEASE_SIGNING_KEY overrides
# it; anywhere but inside this repository); its public half is
# deploy/release.pub.pem, which install.sh and install-cli.sh embed.
#
# Needs zig, cargo-zigbuild and go: every Linux binary is cross-compiled
# natively (static musl for Rust, CGO-free for Caddy), on Linux or macOS.
set -euo pipefail
cd "$(dirname "$0")/.."

CHECK_ONLY=0
case "${1:-}" in
  --check) CHECK_ONLY=1 ;;
  "") ;;
  *) echo "usage: scripts/release.sh [--check]" >&2; exit 2 ;;
esac

ARCHES=${ARCHES:-"amd64 arm64"}
OUT=${OUT:-dist/release}
KEY=${RELEASE_SIGNING_KEY:-$HOME/.config/modula/release-signing/release.key}
CADDY_VERSION=${CADDY_VERSION:-v2.11.1}
XCADDY_VERSION=${XCADDY_VERSION:-v0.4.5}
# DNS-01 for wildcard certificates on generated domains; rate limit and
# cache edge rules. Keep in sync with deploy/dev/node.Dockerfile.
# cache-handler v0.17.0 needs Caddy v2.11.4; v0.16.0 builds with v2.11.1.
CADDY_PLUGINS=(
  github.com/caddy-dns/cloudflare@v0.2.4
  github.com/mholt/caddy-ratelimit@v0.1.1-0.20260612195517-5625512f24f6
  github.com/caddyserver/cache-handler@v0.16.0
)

[[ -f $KEY ]] || { echo "signing key $KEY not found" >&2; exit 1; }
repo=$(pwd -P)
key_real=$(cd "$(dirname "$KEY")" && pwd -P)/$(basename "$KEY")
case $key_real in
  "$repo"/*) echo "signing key $KEY is inside the repository; keep it outside (default ~/.config/modula/release-signing/release.key)" >&2; exit 1 ;;
esac
embedded=$(sed -n 's/^RELEASE_PUBKEY="\${MODULA_RELEASE_PUBKEY:-\(.*\)}"$/\1/p' deploy/install.sh)
actual=$(openssl pkey -in "$KEY" -pubout | sed -n 2p)
[[ $embedded == "$actual" ]] || { echo "install.sh embeds a different public key than $KEY" >&2; exit 1; }
cli_embedded=$(sed -n 's/^RELEASE_PUBKEY="\${MODULA_RELEASE_PUBKEY:-\(.*\)}"$/\1/p' scripts/install-cli.sh)
[[ $cli_embedded == "$actual" ]] || { echo "install-cli.sh embeds a different public key than $KEY" >&2; exit 1; }
cmp -s <(openssl pkey -in "$KEY" -pubout) deploy/release.pub.pem \
  || { echo "deploy/release.pub.pem does not match $KEY" >&2; exit 1; }
if [[ $CHECK_ONLY == 1 ]]; then
  echo "signing key ok: $KEY matches deploy/release.pub.pem, install.sh and install-cli.sh"
  exit 0
fi

mkdir -p "$OUT"
rm -f "$OUT"/SHA256SUMS "$OUT"/SHA256SUMS.sig

echo "web: building the dashboard (embedded into modula-server)"
(cd web && pnpm install --frozen-lockfile >/dev/null && pnpm build >/dev/null)

# Linux binaries are cross-compiled on this machine, never emulated:
# static musl builds with cargo-zigbuild (zig links ring's and SQLite's C).
for tool in zig cargo-zigbuild; do
  command -v "$tool" >/dev/null || { echo "$tool not found (zig from ziglang.org, cargo install --locked cargo-zigbuild)" >&2; exit 1; }
done
triple() { case $1 in amd64) echo x86_64-unknown-linux-musl ;; arm64) echo aarch64-unknown-linux-musl ;; esac; }

for arch in $ARCHES; do
  t=$(triple "$arch")
  rustup target add "$t" >/dev/null
  echo "$arch: compiling modula-server, modula-node and the modula CLI"
  cargo zigbuild -q --release --locked --target "$t" -p modula-server -p modula-node -p modula-cli
  cp "target/$t/release/modula-server" "$OUT/modula-server-linux-$arch"
  cp "target/$t/release/modula-node" "$OUT/modula-node-linux-$arch"
  cp "target/$t/release/modula" "$OUT/modula-linux-$arch"

  caddy_stamp="$CADDY_VERSION ${CADDY_PLUGINS[*]}"
  if [[ -x $OUT/caddy-linux-$arch && $(cat "$OUT/.caddy-linux-$arch.stamp" 2>/dev/null) == "$caddy_stamp" ]]; then
    echo "$arch: Caddy $CADDY_VERSION unchanged, reusing it"
  else
    echo "$arch: building Caddy $CADDY_VERSION with ${CADDY_PLUGINS[*]}"
    with=()
    for p in "${CADDY_PLUGINS[@]}"; do with+=(--with "$p"); done
    command -v go >/dev/null || { echo "go not found" >&2; exit 1; }
    GOBIN=$PWD/target/go-bin go install "github.com/caddyserver/xcaddy/cmd/xcaddy@$XCADDY_VERSION"
    (cd "$(mktemp -d)" && CGO_ENABLED=0 GOOS=linux GOARCH="$arch" \
      "$OLDPWD/target/go-bin/xcaddy" build "$CADDY_VERSION" "${with[@]}" --output "$OLDPWD/$OUT/caddy-linux-$arch")
    echo "$caddy_stamp" >"$OUT/.caddy-linux-$arch.stamp"
  fi

  echo "$arch: fetching buildctl, railpack, wal-g and workerd (pinned sums)"
  scripts/fetch-build-tools.sh "$arch" "$OUT"
done

# The CLI for macOS, built natively (needs a Mac; skipped elsewhere).
if [[ $(uname -s) == Darwin && -z ${SKIP_DARWIN:-} ]]; then
  for t in aarch64-apple-darwin:arm64 x86_64-apple-darwin:amd64; do
    echo "darwin ${t#*:}: compiling the modula CLI"
    rustup target add "${t%%:*}" >/dev/null
    cargo build -q --release --locked -p modula-cli --target "${t%%:*}"
    cp "target/${t%%:*}/release/modula" "$OUT/modula-darwin-${t#*:}"
  done
fi

sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1 >"$OUT/VERSION"
[[ -s $OUT/VERSION ]] || { echo "no version in Cargo.toml" >&2; exit 1; }
cp deploy/install.sh "$OUT/install.sh"
cp scripts/install-cli.sh "$OUT/install-cli.sh"
# Apache-2.0 and MIT: the notices ship with the redistributed Caddy,
# buildctl, Railpack, WAL-G and workerd binaries.
cp LICENSE "$OUT/LICENSE"
cp THIRD_PARTY_NOTICES.md "$OUT/THIRD_PARTY_NOTICES.md"
# shellcheck disable=SC2046 # the darwin CLIs may be absent
(cd "$OUT" && sha256sum VERSION install.sh install-cli.sh LICENSE THIRD_PARTY_NOTICES.md modula-*-linux-* modula-linux-* $(ls modula-darwin-* 2>/dev/null) caddy-linux-* buildctl-linux-* railpack-linux-* wal-g-linux-* workerd-linux-* >SHA256SUMS)

openssl pkeyutl -sign -inkey "$KEY" -rawin -in "$OUT/SHA256SUMS" -out "$OUT/SHA256SUMS.sig"
openssl pkeyutl -verify -pubin -inkey deploy/release.pub.pem -rawin \
  -in "$OUT/SHA256SUMS" -sigfile "$OUT/SHA256SUMS.sig" >/dev/null
echo "release: $OUT ($(cd "$OUT" && ls | tr '\n' ' '))"
echo "publish: CLOUDFLARE_ACCOUNT_ID=… scripts/publish.sh (binaries by checksum, unchanged ones skipped)"
