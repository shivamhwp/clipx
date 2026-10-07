#!/bin/sh
# Install clipx and set it up.
#
#   curl -fsSL https://raw.githubusercontent.com/shivamhwp/clipx/main/install.sh | sh
#
# Arguments after `sh -s --` go to `clipx setup`, for example:
#   curl -fsSL …/install.sh | sh -s -- --relay https://relay.example.com --name mybox
#   curl -fsSL …/install.sh | sh -s -- --tunnel off --public
#
# CLIPX_REPO, CLIPX_VERSION (a tag, default latest), CLIPX_DOWNLOAD_BASE (a mirror) and
# CLIPX_INSTALL_DIR override defaults.
set -eu

REPO="${CLIPX_REPO:-shivamhwp/clipx}"
VERSION="${CLIPX_VERSION:-latest}"

say() { printf '%s\n' "$*"; }
die() { printf 'clipx install: %s\n' "$*" >&2; exit 1; }

os=$(uname -s)
arch=$(uname -m)
case "$os/$arch" in
  Linux/x86_64 | Linux/amd64) target=x86_64-unknown-linux-musl ;;
  Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-musl ;;
  Darwin/arm64) target=aarch64-apple-darwin ;;
  Darwin/x86_64) target=x86_64-apple-darwin ;;
  *) die "no prebuilt binary for $os/$arch; build from source with: cargo install --git https://github.com/$REPO" ;;
esac

if [ -n "${CLIPX_INSTALL_DIR:-}" ]; then
  dir=$CLIPX_INSTALL_DIR
elif [ "$(id -u)" = 0 ]; then
  dir=/usr/local/bin
else
  dir=$HOME/.local/bin
fi
mkdir -p "$dir"

if [ -n "${CLIPX_DOWNLOAD_BASE:-}" ]; then
  base=$CLIPX_DOWNLOAD_BASE
elif [ "$VERSION" = latest ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi

fetch() {
  if command -v curl >/dev/null 2>&1; then curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then wget -qO "$2" "$1"
  else die "need curl or wget"; fi
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
asset="clipx-$target.tar.gz"
say "downloading $asset"
if fetch "$base/$asset" "$tmp/$asset" && fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"; then
  want=$(grep " $asset\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
  got=$(sha256 "$tmp/$asset")
  [ -n "$want" ] && [ "$want" = "$got" ] || die "checksum mismatch for $asset"
  tar -xzf "$tmp/$asset" -C "$tmp"
  install -m 0755 "$tmp/clipx" "$dir/clipx" 2>/dev/null || { cp "$tmp/clipx" "$dir/clipx"; chmod 0755 "$dir/clipx"; }
elif command -v cargo >/dev/null 2>&1; then
  say "no release download; building from source with cargo"
  cargo install --locked --git "https://github.com/$REPO" --root "$tmp/cargo" clipx
  cp "$tmp/cargo/bin/clipx" "$dir/clipx"
else
  die "could not download $asset and cargo is not installed"
fi
say "installed $dir/clipx ($("$dir/clipx" --version))"

case ":$PATH:" in
  *":$dir:"*) ;;
  *) say "add $dir to your PATH:  export PATH=\"$dir:\$PATH\"" ;;
esac

say ""
exec "$dir/clipx" setup "$@"
