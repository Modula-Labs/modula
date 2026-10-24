#!/usr/bin/env bash
# Regenerate the dependency lists in THIRD_PARTY_NOTICES.md (everything between
# the `generated:begin` and `generated:end` markers). The hand-written part above
# the markers is left alone.
#
#   scripts/notices.sh
#
# Needs: cargo, jq, pnpm (web/node_modules installed: `pnpm install --frozen-lockfile`).
set -euo pipefail
cd "$(dirname "$0")/.."

file=THIRD_PARTY_NOTICES.md
begin='<!-- generated:begin (scripts/notices.sh) -->'
end='<!-- generated:end -->'
grep -qF "$begin" "$file" && grep -qF "$end" "$file" || { echo "markers missing in $file" >&2; exit 1; }
[[ -d web/node_modules ]] || { echo "run 'pnpm install --frozen-lockfile' in web/ first" >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# Rust: every third-party package in Cargo.lock (all targets and platforms,
# including build and dev dependencies), so the list over-includes on purpose.
cargo metadata --format-version 1 --locked \
  | jq -r '.workspace_members as $ws
      | [.packages[] | select(.id as $id | $ws | index($id) | not)]
      | sort_by(.name, .version)
      | .[] | "| \(.name) | \(.version) | \(.license // "see crate (license-file)") |"' \
  >"$tmp/rust.md"

# npm: production dependencies of the web UI (bundled into web/dist and embedded
# in modula-server). pnpm 8 reports `version`, pnpm 9+ `versions`.
(cd web && pnpm licenses list --prod --json) \
  | jq -r '[to_entries[] | .key as $lic | .value[]
        | {name, version: (.version // (.versions | join(", "))), license: (.license // $lic)}]
      | sort_by(.name, .version)
      | .[] | "| \(.name) | \(.version) | \(.license) |"' \
  >"$tmp/npm.md"

{
  echo "$begin"
  echo
  echo "## Generated: Rust crates"
  echo
  echo "Every third-party crate in \`Cargo.lock\` ($(wc -l <"$tmp/rust.md" | tr -d ' ') packages), from"
  echo "\`cargo metadata\`. This includes build-time and dev-only crates and crates for other"
  echo "platforms, so it is a superset of what any one binary contains. Full license texts ship"
  echo "with each crate's source on crates.io."
  echo
  echo "| Crate | Version | License |"
  echo "| --- | --- | --- |"
  cat "$tmp/rust.md"
  echo
  echo "## Generated: web UI (npm, production dependencies)"
  echo
  echo "From \`pnpm licenses list --prod\` in \`web/\` ($(wc -l <"$tmp/npm.md" | tr -d ' ') packages). These are"
  echo "bundled into the web UI that modula-server embeds."
  echo
  echo "| Package | Version | License |"
  echo "| --- | --- | --- |"
  cat "$tmp/npm.md"
  echo
  echo "$end"
} >"$tmp/generated.md"

awk -v begin="$begin" -v end="$end" -v gen="$tmp/generated.md" '
  $0 == begin { while ((getline line < gen) > 0) print line; skip = 1; next }
  $0 == end   { skip = 0; next }
  !skip       { print }
' "$file" >"$tmp/out.md"
cat "$tmp/out.md" >"$file"
echo "notices: $(wc -l <"$tmp/rust.md" | tr -d ' ') crates, $(wc -l <"$tmp/npm.md" | tr -d ' ') npm packages"
