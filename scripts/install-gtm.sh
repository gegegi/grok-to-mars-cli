#!/usr/bin/env bash
# Build this repo's CLI and install it as `gtm` without touching the official
# `grok` binary in ~/.grok/bin.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
bin_dir="${GTM_BIN_DIR:-$HOME/.local/bin}"
dest="${bin_dir}/gtm"

if [[ "$(basename "$dest")" != "gtm" ]]; then
  echo "error: destination must be named gtm (got ${dest})" >&2
  exit 1
fi

# Official installer / `grok update` own this directory.
case "$bin_dir" in
  */.grok/bin | */.grok/bin/)
    echo "error: refusing to install into ${bin_dir} (official grok lives there)" >&2
    exit 1
    ;;
esac

if [[ -e "$dest" && ! -f "$dest" ]]; then
  echo "error: ${dest} exists and is not a regular file" >&2
  exit 1
fi

export PATH="${HOME}/.cargo/bin:${PATH}"
if ! command -v dotslash >/dev/null 2>&1; then
  echo "error: dotslash is required (cargo install dotslash)" >&2
  exit 1
fi

echo "building gtm (release)…"
cargo build -p xai-grok-pager-bin --release --bin gtm --manifest-path "${root}/Cargo.toml"

built="${root}/target/release/gtm"
if [[ ! -x "$built" ]]; then
  echo "error: expected binary missing: ${built}" >&2
  exit 1
fi

mkdir -p "$bin_dir"
tmp="${dest}.tmp.$$"
cp "$built" "$tmp"
chmod 755 "$tmp"
mv "$tmp" "$dest"

echo "installed ${dest}"
if command -v grok >/dev/null 2>&1; then
  echo "grok is unchanged: $(command -v grok)"
else
  echo "grok is not on PATH (official install is separate)"
fi
if ! command -v gtm >/dev/null 2>&1; then
  echo "note: add ${bin_dir} to PATH to run \`gtm\`"
fi
