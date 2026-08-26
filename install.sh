#!/bin/sh
# Install jqf. Prefers crates.io (`cargo install jqf` → ~/.cargo/bin, usually on PATH).
# Without cargo, fetches the latest GitHub release binary into $PREFIX (default: ~/.local/bin).
set -eu
REPO="${JQF_REPO:-filip-41/jqf}"

if command -v cargo >/dev/null 2>&1; then
  echo "install.sh: cargo install jqf" >&2
  cargo install jqf
  if command -v jqf >/dev/null 2>&1; then
    jqf --version
  else
    echo "install.sh: installed; add \$HOME/.cargo/bin to PATH" >&2
    "$HOME/.cargo/bin/jqf" --version
  fi
  exit 0
fi

PREFIX="${PREFIX:-$HOME/.local/bin}"
os=$(uname -s)
arch=$(uname -m)
case "$os-$arch" in
  Darwin-arm64) asset=jqf-aarch64-apple-darwin ;;
  Darwin-x86_64) asset=jqf-x86_64-apple-darwin ;;
  Linux-x86_64) asset=jqf-x86_64-unknown-linux-gnu ;;
  Linux-aarch64|Linux-arm64) asset=jqf-aarch64-unknown-linux-gnu ;;
  *)
    echo "install.sh: no binary for $os $arch and cargo is not installed" >&2
    echo "install.sh: install Rust from https://rustup.rs then re-run, or: brew tap filip-41/jqf && brew install jqf" >&2
    exit 1
    ;;
esac
url="https://github.com/${REPO}/releases/latest/download/${asset}"
mkdir -p "$PREFIX"
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
echo "install.sh: no cargo; fetching $url" >&2
curl -fsSL "$url" -o "$tmp"
chmod +x "$tmp"
mv "$tmp" "$PREFIX/jqf"
trap - EXIT
echo "install.sh: installed $PREFIX/jqf" >&2
case ":$PATH:" in
  *":$PREFIX:"*) ;;
  *)
    echo "install.sh: add this to your shell rc, then open a new terminal:" >&2
    echo "  export PATH=\"$PREFIX:\$PATH\"" >&2
    ;;
esac
"$PREFIX/jqf" --version
