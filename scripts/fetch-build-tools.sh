#!/usr/bin/env bash
# Download the tools modula-node drives (BuildKit's buildctl, Railpack,
# WAL-G for Postgres point-in-time recovery, and workerd for functions) for
# one architecture, checked against pinned SHA-256 sums.
#
#   scripts/fetch-build-tools.sh <amd64|arm64> <out-dir>
#
# Writes <out-dir>/buildctl-linux-<arch>, <out-dir>/railpack-linux-<arch>,
# <out-dir>/wal-g-linux-<arch> and <out-dir>/workerd-linux-<arch>. Bump
# versions and sums together (the buildkit image digest lives in
# crates/node/src/build/buildkit.rs; workerd's release date is the newest
# compatibility date functions may use, WORKERD_COMPATIBILITY_DATE in
# crates/server/src/deploy/config.rs).
#
# WAL-G's Postgres build links glibc dynamically (libsodium and brotli are
# static): the Ubuntu 20.04 one (glibc 2.31) runs in the Debian-based
# postgres images it is mounted into.
set -euo pipefail

arch=${1:?arch}
out=${2:?out dir}
BUILDKIT_VERSION=v0.34.1
RAILPACK_VERSION=v0.43.0
WALG_VERSION=v3.0.9
WORKERD_VERSION=v1.20261014.1

case $arch in
  amd64)
    bk_sum=a59539daf547fe0958efe2d07e41c0a6944e1112a66d7e519ec1cf5b673ef15b
    rp_arch=x86_64 rp_sum=248a8f60f36854aabed6124113631078a802de3461660855fc4de9c26065c1fb
    wg_arch=amd64 wg_sum=965a7147852435bc4b54f272799b63df826b54ef65cf7c4fc688c82b96c0c4eb
    wd_arch=64 wd_sum=f4fcdafea302598cc1dfd7ab155b50340019cd52bce9472553ba84d31283ba5b ;;
  arm64)
    bk_sum=c8c97b84baf89dfe0ea85063ac24ad88eb909979ea476ce3ef9e8e11e60ebef8
    rp_arch=arm64 rp_sum=732ece14eb4f4676a43ebba19431f954edf7b130acb76159ef0f2ff86ac42ac9
    wg_arch=aarch64 wg_sum=89f0cea535eed8c5b742358bff5151e65b302eab90689d0afb38750f8d505936
    wd_arch=arm64 wd_sum=faebc0158840812619692932f375ee2c8ab1b2a9ef90141df44ba70e10a83f71 ;;
  *) echo "unsupported arch $arch" >&2; exit 1 ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$out"

# Pinned versions: tools fetched for the same pins are the same files, so
# they're only downloaded again when a pin changes.
stamp="$out/.tools-linux-$arch.stamp"
pins="$BUILDKIT_VERSION $bk_sum $RAILPACK_VERSION $rp_sum $WALG_VERSION $wg_sum $WORKERD_VERSION $wd_sum"
if [[ $(cat "$stamp" 2>/dev/null) == "$pins" ]] \
  && [[ -x $out/buildctl-linux-$arch && -x $out/railpack-linux-$arch && -x $out/wal-g-linux-$arch && -x $out/workerd-linux-$arch ]]; then
  echo "$arch: build tools unchanged, reusing them"
  exit 0
fi

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

fetch "https://github.com/wal-g/wal-g/releases/download/$WALG_VERSION/wal-g-pg-20.04-$wg_arch" "$wg_sum" "$tmp/wal-g"
install -m 0755 "$tmp/wal-g" "$out/wal-g-linux-$arch"

# The sum is the release asset's (gzipped).
fetch "https://github.com/cloudflare/workerd/releases/download/$WORKERD_VERSION/workerd-linux-$wd_arch.gz" "$wd_sum" "$tmp/workerd.gz"
gunzip "$tmp/workerd.gz"
install -m 0755 "$tmp/workerd" "$out/workerd-linux-$arch"
echo "$pins" >"$stamp"
