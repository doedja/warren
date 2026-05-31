#!/bin/sh
# warren node installer.
#   curl -fsSL https://raw.githubusercontent.com/doedja/warren/main/install.sh | sh
#   ... | sh -s -- --hub HOST:7000 --token TOKEN [--tls --hub-fingerprint FP] [--name NAME]
# With args, it installs the binary and registers a boot service. Without args,
# it just installs the binary and prints the next step.
set -e

REPO="doedja/warren"

os="$(uname -s)"
arch="$(uname -m)"

fallback() {
  echo "warren: no prebuilt binary for $os/$arch." >&2
  echo "On Windows, use install.ps1. Otherwise build from source:" >&2
  echo "  cargo install --git https://github.com/$REPO warren" >&2
  exit 1
}

case "$os" in
  Linux)
    case "$arch" in
      x86_64) target="x86_64-unknown-linux-musl" ;;
      aarch64 | arm64) target="aarch64-unknown-linux-musl" ;;
      *) fallback ;;
    esac
    ;;
  Darwin)
    case "$arch" in
      x86_64) target="x86_64-apple-darwin" ;;
      arm64 | aarch64) target="aarch64-apple-darwin" ;;
      *) fallback ;;
    esac
    ;;
  *) fallback ;;
esac

url="https://github.com/$REPO/releases/latest/download/warren-$target.tar.gz"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "warren: downloading $url" >&2
if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$url" -o "$tmp/warren.tar.gz"
elif command -v wget >/dev/null 2>&1; then
  wget -qO "$tmp/warren.tar.gz" "$url"
else
  echo "warren: need curl or wget." >&2
  exit 1
fi

tar -xzf "$tmp/warren.tar.gz" -C "$tmp"
bin="$(find "$tmp" -type f -name warren | head -n 1)"
if [ -z "$bin" ]; then
  echo "warren: binary not found in archive." >&2
  exit 1
fi
chmod +x "$bin"

# Pick an install location.
sudo=""
if [ "$(id -u)" != "0" ] && command -v sudo >/dev/null 2>&1; then
  sudo="sudo"
fi

dest="/usr/local/bin/warren"
if [ -w /usr/local/bin ] || [ "$(id -u)" = "0" ]; then
  cp "$bin" "$dest"
elif [ -n "$sudo" ]; then
  $sudo cp "$bin" "$dest"
else
  mkdir -p "$HOME/.local/bin"
  dest="$HOME/.local/bin/warren"
  cp "$bin" "$dest"
  echo "warren: installed to $dest (add \$HOME/.local/bin to PATH)." >&2
fi
echo "warren: installed at $dest" >&2

if [ "$#" -gt 0 ]; then
  # Register a boot service with the passed args (needs root for systemd).
  echo "warren: installing node service..." >&2
  if [ "$(id -u)" = "0" ]; then
    "$dest" node install "$@"
  elif [ -n "$sudo" ]; then
    $sudo "$dest" node install "$@"
  else
    "$dest" node install "$@"
  fi
else
  echo "" >&2
  echo "Next: run the node (Mode B with a token, or Mode A and approve it in the dashboard):" >&2
  echo "  warren node run --hub HOST:7000 --token TOKEN [--tls --hub-fingerprint FP]" >&2
  echo "  warren node run --hub HOST:7000            # no token: shows a pending code to approve" >&2
fi
