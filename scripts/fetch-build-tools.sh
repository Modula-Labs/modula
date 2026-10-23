#!/usr/bin/env bash
# Download the build tools modula-node drives (BuildKit's buildctl, Railpack)
# for one architecture, checked against pinned SHA-256 sums.
#
#   scripts/fetch-build-tools.sh <amd64|arm64> <out-dir>
#
# Writes <out-dir>/buildctl-linux-<arch> and <out-dir>/railpack-linux-<arch>.
# Bump versions and sums together (the buildkit image digest lives in
# crates/node/src/build/buildkit.rs).
set -euo pipefail

arch=${1:?arch}
out=${2:?out dir}
BUILDKIT_VERSION=v0.34.1
RAILPACK_VERSION=v0.43.0

case $arch in
  amd64)
    bk_sum=a59539daf547fe0958efe2d07e41c0a6944e1112a66d7e519ec1cf5b673ef15b
    rp_arch=x86_64 rp_sum=248a8f60f36854aabed6124113631078a802de3461660855fc4de9c26065c1fb ;;
  arm64)
    bk_sum=c8c97b84baf89dfe0ea85063ac24ad88eb909979ea476ce3ef9e8e11e60ebef8
    rp_arch=arm64 rp_sum=732ece14eb4f4676a43ebba19431f954edf7b130acb76159ef0f2ff86ac42ac9 ;;
  *) echo "unsupported arch $arch" >&2; exit 1 ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$out"

fetch() { # url sha256 dest
  curl -fsSL --retry 3 -o "$3" "$1"
  echo "$2  $3" | sha256sum --check --quiet - || { echo "checksum mismatch for $1" >&2; exit 1; }
}

fetch "https://github.com/moby/buildkit/releases/download/$BUILDKIT_VERSION/buildkit-$BUILDKIT_VERSION.linux-$arch.tar.gz" "$bk_sum" "$tmp/buildkit.tgz"
tar -xzf "$tmp/buildkit.tgz" -C "$tmp" bin/buildctl
install -m 0755 "$tmp/bin/buildctl" "$out/buildctl-linux-$arch"

fetch "https://github.com/railwayapp/railpack/releases/download/$RAILPACK_VERSION/railpack-$RAILPACK_VERSION-$rp_arch-unknown-linux-musl.tar.gz" "$rp_sum" "$tmp/railpack.tgz"
tar -xzf "$tmp/railpack.tgz" -C "$tmp" railpack
install -m 0755 "$tmp/railpack" "$out/railpack-linux-$arch"
