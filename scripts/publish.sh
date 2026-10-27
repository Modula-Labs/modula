#!/usr/bin/env bash
# Publish dist/release (scripts/release.sh) to the release bucket (R2).
#
#   CLOUDFLARE_ACCOUNT_ID=… scripts/publish.sh
#
# Binaries are stored once, by checksum: releases/files/<sha256>. One already
# there (Caddy, buildctl and Railpack rarely change; unchanged builds of ours
# neither) is not uploaded again. A release itself is only its signed
# SHA256SUMS (which names every file and its sum) plus the install scripts,
# under releases/download/v<version>/ and releases/latest/download/. VERSION
# goes last: update checks see a release only once all of it is there.
set -euo pipefail
cd "$(dirname "$0")/.."

OUT=${OUT:-dist/release}
BUCKET=${BUCKET:-modula-releases}
PUBLIC=${PUBLIC:-https://downloads.usemodula.org/releases}
JOBS=${JOBS:-6}
version=$(cat "$OUT/VERSION")
[[ -n ${CLOUDFLARE_ACCOUNT_ID:-} ]] || { echo "set CLOUDFLARE_ACCOUNT_ID" >&2; exit 1; }

put() { # put KEY FILE
  npx -y wrangler@4 r2 object put "$BUCKET/$1" --file "$2" --remote >/dev/null 2>&1 \
    || { echo "upload failed: $1" >&2; return 1; }
}
export -f put
export BUCKET

# Binaries by checksum, skipping those already published.
todo=$(mktemp)
trap 'rm -f "$todo"' EXIT
while read -r sum name; do
  name=${name#\*}
  [[ $name == VERSION ]] && continue
  if curl -fsSI -o /dev/null "$PUBLIC/files/$sum"; then
    echo "have  $name"
  else
    echo "new   $name ($(du -h "$OUT/$name" | cut -f1))"
    echo "releases/files/$sum $OUT/$name" >>"$todo"
  fi
done <"$OUT/SHA256SUMS"
if [[ -s $todo ]]; then
  xargs -P "$JOBS" -L 1 bash -c 'put "$0" "$1"' <"$todo"
fi

for p in "releases/download/v$version" releases/latest/download; do
  for f in SHA256SUMS SHA256SUMS.sig install.sh install-cli.sh; do put "$p/$f" "$OUT/$f"; done
done
for p in "releases/download/v$version" releases/latest/download; do put "$p/VERSION" "$OUT/VERSION"; done
echo "published $version"
