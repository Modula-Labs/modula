#!/usr/bin/env bash
# Install the modula CLI (Linux and macOS, amd64/arm64) on a workstation or CI.
#
#   curl -fsSL https://usemodula.org/install-cli.sh | bash
#   curl -fsSL https://usemodula.org/install-cli.sh | bash -s -- --version 0.1.0 --dir /usr/local/bin
#
# Downloads the prebuilt static binary and checks it against SHA256SUMS signed
# with the release key (the same key and checks as deploy/install.sh). No root
# needed: installs to ~/.local/bin unless --dir says otherwise. Where there is
# no prebuilt binary, build it from source: cargo install --path crates/cli
#
# Runs on the bash 3.2 that macOS ships, so no bash 4 features below.
set -euo pipefail

VERSION="${MODULA_VERSION:-latest}"
RELEASE_URL="${MODULA_RELEASE_URL:-https://downloads.usemodula.org/releases}"
# Ed25519 key that signs SHA256SUMS (deploy/release.pub.pem); release.sh checks it matches.
RELEASE_PUBKEY="${MODULA_RELEASE_PUBKEY:-MCowBQYDK2VwAyEAVl1B0m1qDZY+gGHSLVry769Tl1KmGKKFhFiav9ccXCE=}"
DIR="${MODULA_CLI_DIR:-$HOME/.local/bin}"

# ---------------------------------------------------------------- output
# Teal on terminals (24-bit when the terminal says so, else 256 colors); plain
# text when piped, logged, or with NO_COLOR set.

A="" B="" D="" R="" N=""
if [[ -t 1 && -z ${NO_COLOR:-} && ${TERM:-} != dumb ]]; then
  case ${COLORTERM:-} in
    truecolor | 24bit) A=$'\e[38;2;45;212;191m' ;;
    *) A=$'\e[38;5;43m' ;;
  esac
  B=$'\e[1m' D=$'\e[2m' R=$'\e[31m' N=$'\e[0m'
fi
TTY=0
[[ -t 1 && ${TERM:-} != dumb ]] && TTY=1

say() { printf '%s\n' "$*"; }
# die "what went wrong" ["what to do about it"]
die() {
  [[ $TTY == 1 ]] && printf '\r\033[K'
  printf '  %s✗ %s%s\n' "$R" "$1" "$N" >&2
  [[ -n ${2:-} ]] && printf '    %s\n' "$2" >&2
  exit 1
}
# A step: "→ label…" while it runs (on terminals), then "✓ label".
begin() { [[ $TTY == 1 ]] && printf '  %s→%s %s…' "$D" "$N" "$1"; return 0; }
done_() {
  [[ $TTY == 1 ]] && printf '\r\033[K'
  printf '  %s✓%s %s\n' "$A" "$N" "$1"
}

# ---------------------------------------------------------------- arguments

while [[ $# -gt 0 ]]; do
  case $1 in
    --version) VERSION=${2:?--version needs a value}; shift 2 ;;
    --dir) DIR=${2:?--dir needs a value}; shift 2 ;;
    *) die "unknown option $1" "This script takes --version X.Y.Z and --dir PATH." ;;
  esac
done

say ""
say "  ${A}${B}◆ modula${N}  ${D}command-line tools for Modula Deploy${N}"
say "  Installing the modula CLI…"
say ""

case $(uname -s) in
  Linux) OS=linux ;;
  Darwin) OS=darwin ;;
  *) die "there is no prebuilt modula CLI for $(uname -s) yet" \
    "You can build it from source with Rust: cargo install --path crates/cli" ;;
esac
case $(uname -m) in
  x86_64) ARCH=amd64 ;;
  aarch64 | arm64) ARCH=arm64 ;;
  *) die "there is no prebuilt modula CLI for $(uname -m) CPUs yet" \
    "You can build it from source with Rust: cargo install --path crates/cli" ;;
esac
command -v curl >/dev/null || die "curl is required" "Install curl with your package manager, then run this again."

# Ed25519 signatures need OpenSSL 1.1.1+ (`pkeyutl -rawin`). macOS's built-in
# LibreSSL can't check them, so look for Homebrew's OpenSSL too.
OPENSSL=""
for c in "$(command -v openssl || true)" /opt/homebrew/opt/openssl*/bin/openssl /usr/local/opt/openssl*/bin/openssl; do
  [[ -n $c && -x $c ]] || continue
  if "$c" pkeyutl -help 2>&1 | grep -q -- -rawin; then OPENSSL=$c; break; fi
done
if [[ -z $OPENSSL ]]; then
  if [[ $OS == darwin ]]; then
    die "this Mac's built-in openssl can't check the release signature" \
      "Install a newer OpenSSL with Homebrew (brew install openssl), then run this again."
  fi
  die "OpenSSL 1.1.1 or newer is required to check the release signature" \
    "Install openssl with your package manager, then run this again."
fi
if command -v sha256sum >/dev/null; then
  sha256() { sha256sum "$1"; }
elif command -v shasum >/dev/null; then
  sha256() { shasum -a 256 "$1"; }
else
  die "sha256sum (or shasum) is required" "Install coreutils with your package manager, then run this again."
fi

# ---------------------------------------------------------------- download

name="modula-$OS-$ARCH"
# How to show DIR: ~/… on screen, $HOME/… in the PATH line to copy.
show_dir=$DIR export_dir=$DIR
case $DIR in
  "$HOME"/*) show_dir='~'/"${DIR#"$HOME"/}" export_dir="\$HOME/${DIR#"$HOME"/}" ;;
esac
if [[ $VERSION == latest ]]; then base="$RELEASE_URL/latest/download"; else base="$RELEASE_URL/download/v$VERSION"; fi
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

begin "Checking the release ($VERSION)"
for f in SHA256SUMS SHA256SUMS.sig; do
  curl -fsSL --retry 3 -o "$tmp/$f" "$base/$f" \
    || die "couldn't download $base/$f" "Check your connection (and the version, if you passed one), then try again."
done
printf -- "-----BEGIN PUBLIC KEY-----\n%s\n-----END PUBLIC KEY-----\n" "$RELEASE_PUBKEY" >"$tmp/release.pub.pem"
"$OPENSSL" pkeyutl -verify -pubin -inkey "$tmp/release.pub.pem" -rawin \
  -in "$tmp/SHA256SUMS" -sigfile "$tmp/SHA256SUMS.sig" >/dev/null 2>&1 \
  || die "SHA256SUMS signature is not valid; refusing to install" \
    "Something between you and the release server changed the files. Nothing was installed."
done_ "Release signature checked"

# The signed list says which binaries this release has.
expected=$(awk -v n="$name" '$2 == n || $2 == "*" n { print $1; exit }' "$tmp/SHA256SUMS")
if [[ -z $expected ]]; then
  [[ $TTY == 1 ]] && printf '\r\033[K'
  say ""
  say "  ${A}╭─${N} ${B}No prebuilt binary for $OS/$ARCH in this release (yet!)${N}"
  say "  ${A}│${N}"
  say "  ${A}│${N}  You can still get the CLI by building it from source with Rust"
  say "  ${A}│${N}  (${D}https://rustup.rs${N}), from a checkout of the Modula source:"
  say "  ${A}│${N}"
  say "  ${A}│${N}    ${A}cargo install --path crates/cli${N}"
  say "  ${A}│${N}"
  say "  ${A}╰─${N} ${D}Nothing was installed. Sorry for the detour!${N}"
  say ""
  exit 1
fi

begin "Downloading $name"
# Releases store binaries once, by checksum (files/<sha256>).
curl -fsSL --retry 3 -o "$tmp/$name" "$RELEASE_URL/files/$expected" \
  || die "couldn't download $name ($RELEASE_URL/files/$expected)" "Check your connection, then try again."
actual=$(sha256 "$tmp/$name" | awk '{ print $1 }')
[[ $actual == "$expected" ]] || die "checksum mismatch; refusing to install" \
  "The download doesn't match the signed SHA256SUMS. Nothing was installed."
done_ "Downloaded $name (checksum verified)"

begin "Installing to $show_dir"
mkdir -p "$DIR"
install -m 0755 "$tmp/$name" "$DIR/modula"
installed=$("$DIR/modula" --version)
done_ "Installed $installed to $show_dir/modula"

# ---------------------------------------------------------------- done

on_path=1
case ":$PATH:" in
  *":$DIR:"*) ;;
  *) on_path=0 ;;
esac
say ""
say "  ${A}╭─${N} ${B}You're all set!${N} ${D}The modula CLI is ready.${N}"
say "  ${A}│${N}"
if [[ $on_path == 0 ]]; then
  say "  ${A}│${N}  ${B}$show_dir${N} isn't on your PATH yet. Add this to your shell profile:"
  say "  ${A}│${N}    export PATH=\"$export_dir:\$PATH\""
  say "  ${A}│${N}"
fi
say "  ${A}│${N}  Next, either connect it to your Modula server:"
say "  ${A}│${N}    ${A}modula login${N}"
say "  ${A}│${N}  or create a new one on Hetzner Cloud from here:"
say "  ${A}│${N}    ${A}modula server create${N}"
say "  ${A}│${N}"
say "  ${A}│${N}  ${D}People and AI agents alike: ${N}${A}modula guide${N}"
say "  ${A}╰─${N} ${D}Happy shipping!${N}"
say ""

# ---------------------------------------------------------------- what next
# Only with someone to ask: piped into bash, stdin is this script, so the
# answers come from the terminal itself. Without one (CI, agents), the card
# above already said both commands.

[[ $TTY == 1 ]] || exit 0
{ exec 5</dev/tty; } 2>/dev/null || exit 0
# The commands read keys through the terminal's own device (/dev/ttys003),
# not the /dev/tty alias: macOS can't watch /dev/tty for input.
dev=/dev/$(ps -o tty= -p $$ 2>/dev/null | tr -d ' ')
[[ -c $dev && $dev != /dev/tty ]] && { exec 5<>"$dev"; } 2>/dev/null
modula=$DIR/modula
say "  ${A}?${N} ${B}What next?${N}"
say "    ${A}1${N}  Connect to an existing Modula server   ${D}(modula login)${N}"
say "    ${A}2${N}  Create a new server on Hetzner         ${D}(modula server create)${N}"
say "    ${A}3${N}  Later"
choice=""
while :; do
  printf '  %s›%s Choose 1, 2 or 3 %s[3]%s ' "$A" "$N" "$D" "$N"
  IFS= read -r -u 5 choice || { say ""; exit 0; }
  case ${choice:-3} in
    1 | 2 | 3) break ;;
    *) say "    ${R}Please answer 1, 2 or 3.${N}" ;;
  esac
done
case ${choice:-3} in
  1)
    say ""
    exec "$modula" login <&5
    ;;
  2)
    say ""
    exec "$modula" server create <&5
    ;;
  *)
    say ""
    say "  ${D}Whenever you're ready: modula login  or  modula server create${N}"
    say ""
    ;;
esac
