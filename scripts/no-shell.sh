#!/usr/bin/env bash
# Fails if any Rust source spawns a shell or builds a shell command string.
# User-controlled values must only ever reach processes as separate argv items.
# A reviewed line that runs a fixed script with no user input can opt out with
# a `no-shell: allow (<reason>)` comment on the same line.
set -euo pipefail
cd "$(dirname "$0")/.."

pattern='Command::new\("(/bin/|/usr/bin/)?(ba|z|da)?sh"\)|"(ba|z|da)?sh -c|"-c",'
if grep -rnE "$pattern" --include='*.rs' crates/ | grep -v 'no-shell: allow ('; then
  echo "error: shell invocation found; pass arguments as argv (Command::new(bin).args([...]))" >&2
  exit 1
fi
echo "no-shell: ok"
