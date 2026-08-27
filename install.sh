#!/bin/sh
# Install jqf. Uses a checksummed PGO release archive when one exists, otherwise
# falls back to the source package on crates.io.
set -eu
REPO="${JQF_REPO:-filip-41/jqf}"
PREFIX="${PREFIX:-$HOME/.local/bin}"

install_cargo() {
  if ! command -v cargo >/dev/null 2>&1; then
    echo "install.sh: no GitHub binary for this platform and cargo is not installed" >&2
    echo "install.sh: install Rust from https://rustup.rs then re-run, or: brew tap filip-41/jqf && brew install jqf" >&2
    exit 1
  fi
  echo "install.sh: cargo install jqf (latest source release, not PGO)" >&2
  cargo install jqf
  if command -v jqf >/dev/null 2>&1; then
    jqf --version
  else
    echo "install.sh: installed; add \$HOME/.cargo/bin to PATH" >&2
    "$HOME/.cargo/bin/jqf" --version
  fi
  exit 0
}

sha256_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    echo "install.sh: no SHA-256 tool (shasum or sha256sum)" >&2
    return 1
  fi
}

os=$(uname -s)
arch=$(uname -m)
case "$os-$arch" in
  Darwin-arm64) asset=jqf-aarch64-apple-darwin ;;
  Darwin-x86_64) asset=jqf-x86_64-apple-darwin ;;
  Linux-x86_64) asset=jqf-x86_64-unknown-linux-gnu ;;
  Linux-aarch64|Linux-arm64) asset=jqf-aarch64-unknown-linux-gnu ;;
  *) install_cargo ;;
esac

archive="${asset}.tar.gz"
url="https://github.com/${REPO}/releases/latest/download/${archive}"
mkdir -p "$PREFIX"
tmp=$(mktemp -d "$PREFIX/.jqf-install.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
echo "install.sh: fetching $url" >&2
if ! curl -fsSL "$url" -o "$tmp/$archive" \
  || ! curl -fsSL "$url.sha256" -o "$tmp/$archive.sha256"; then
  echo "install.sh: checksummed GitHub archive unavailable; falling back to cargo" >&2
  trap - EXIT
  rm -rf "$tmp"
  install_cargo
fi
expected=$(awk 'NR == 1 {print $1}' "$tmp/$archive.sha256")
actual=$(sha256_file "$tmp/$archive") || exit 1
if [ -z "$expected" ] || [ "$actual" != "$expected" ]; then
  echo "install.sh: checksum mismatch for $archive" >&2
  exit 1
fi
tar -xzf "$tmp/$archive" -C "$tmp"
candidate="$tmp/$asset/jqf"
if [ ! -x "$candidate" ] \
  || ! "$candidate" --diagnostics -n 'null' </dev/null 2>&1 | grep -Fq 'jqf: build=pgo '; then
  echo "install.sh: archive does not contain a working PGO jqf" >&2
  exit 1
fi
mv "$candidate" "$PREFIX/jqf"
trap - EXIT
rm -rf "$tmp"
echo "install.sh: installed $PREFIX/jqf" >&2
case ":$PATH:" in
  *":$PREFIX:"*) ;;
  *)
    echo "install.sh: add this to your shell rc, then open a new terminal:" >&2
    echo "  export PATH=\"$PREFIX:\$PATH\"" >&2
    ;;
esac
"$PREFIX/jqf" --version
