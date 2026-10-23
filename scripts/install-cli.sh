#!/usr/bin/env bash
# Install the modula CLI (Linux amd64/arm64) on a workstation or CI.
#
#   curl -fsSL <release url>/install-cli.sh | bash
#   curl -fsSL <release url>/install-cli.sh | bash -s -- --version 0.1.0 --dir /usr/local/bin
#
# Downloads the prebuilt static binary and checks it against SHA256SUMS signed
# with the release key (the same key and checks as deploy/install.sh). No root
# needed: installs to ~/.local/bin unless --dir says otherwise. On other
# systems, build it from source: cargo install --path crates/cli
set -euo pipefail

VERSION="${MODULA_VERSION:-latest}"
RELEASE_URL="${MODULA_RELEASE_URL:-https://github.com/modula-deploy/modula-deploy/releases}"
# Ed25519 key that signs SHA256SUMS (deploy/release.pub.pem); release.sh checks it matches.
RELEASE_PUBKEY="${MODULA_RELEASE_PUBKEY:-MCowBQYDK2VwAyEAEofOHNvogrpURvg/JDfOV83WE6+yAoXsdXvjJlOp/xI=}"
DIR="${MODULA_CLI_DIR:-$HOME/.local/bin}"

die() { echo "error: $*" >&2; exit 1; }

while [[ $# -gt 0 ]]; do
  case $1 in
    --version) VERSION=${2:?--version needs a value}; shift 2 ;;
    --dir) DIR=${2:?--dir needs a value}; shift 2 ;;
    *) die "unknown option $1" ;;
  esac
done

[[ $(uname -s) == Linux ]] || die "prebuilt binaries are Linux-only for now; build from source: cargo install --path crates/cli"
case $(uname -m) in
  x86_64) ARCH=amd64 ;;
  aarch64 | arm64) ARCH=arm64 ;;
  *) die "unsupported CPU architecture $(uname -m)" ;;
esac
for c in curl openssl sha256sum; do command -v "$c" >/dev/null || die "$c is required"; done

name="modula-linux-$ARCH"
if [[ $VERSION == latest ]]; then base="$RELEASE_URL/latest/download"; else base="$RELEASE_URL/download/v$VERSION"; fi
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
for f in SHA256SUMS SHA256SUMS.sig "$name"; do
  curl -fsSL --retry 3 -o "$tmp/$f" "$base/$f" || die "cannot download $base/$f"
done

printf -- "-----BEGIN PUBLIC KEY-----\n%s\n-----END PUBLIC KEY-----\n" "$RELEASE_PUBKEY" >"$tmp/release.pub.pem"
openssl pkeyutl -verify -pubin -inkey "$tmp/release.pub.pem" -rawin \
  -in "$tmp/SHA256SUMS" -sigfile "$tmp/SHA256SUMS.sig" >/dev/null \
  || die "SHA256SUMS signature is not valid; refusing to install"
grep -q "  $name\$" "$tmp/SHA256SUMS" || die "$name is not in SHA256SUMS"
(cd "$tmp" && sha256sum --check --ignore-missing --quiet SHA256SUMS) \
  || die "checksum mismatch; refusing to install"

mkdir -p "$DIR"
install -m 0755 "$tmp/$name" "$DIR/modula"
echo "installed $("$DIR/modula" --version) to $DIR/modula"
case ":$PATH:" in
  *":$DIR:"*) ;;
  *) echo "note: $DIR is not on your PATH" ;;
esac
echo "next: modula login --url https://your-instance"
