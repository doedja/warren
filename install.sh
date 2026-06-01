#!/bin/sh
# warren node installer.
#   curl -fsSL https://raw.githubusercontent.com/doedja/warren/main/install.sh | sh
#   ... | sh -s -- --hub HOST:7000 --token TOKEN [--tls --hub-fingerprint FP] [--name NAME]
# With args, it installs the binary and registers a boot service. Without args,
# it just installs the binary and prints the next step.
set -e

REPO="doedja/warren"

# Clean uninstall: stop the service, remove the key, remove the binary.
if [ "$1" = "--uninstall" ] || [ "$1" = "uninstall" ]; then
  if command -v warren >/dev/null 2>&1; then
    warren node uninstall || true
  else
    echo "warren: binary not on PATH; nothing to run. Remove it manually if present." >&2
  fi
  echo "warren: uninstalled." >&2
  exit 0
fi

os="$(uname -s)"
arch="$(uname -m)"

fallback() {
  echo "warren: no prebuilt binary for $os/$arch." >&2
  echo "On Windows, use install.ps1. Otherwise build from source:" >&2
  echo "  cargo install --git https://github.com/$REPO warren" >&2
  exit 1
}

# asset = warren-<os>-<arch>.tar.gz (see .github/workflows/release.yml)
case "$os" in
  Linux)
    case "$arch" in
      x86_64) asset="linux-x86_64" ;;
      aarch64 | arm64) asset="linux-arm64" ;;
      *) fallback ;;
    esac
    ;;
  Darwin)
    case "$arch" in
      x86_64) asset="macos-x86_64" ;;
      arm64 | aarch64) asset="macos-arm64" ;;
      *) fallback ;;
    esac
    ;;
  *) fallback ;;
esac

url="https://github.com/$REPO/releases/latest/download/warren-$asset.tar.gz"
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

# If a node service is already running, stop it first so its binary can be
# replaced (a running executable cannot be overwritten: ETXTBSY on Linux). It
# is restarted below by `node install`.
if command -v systemctl >/dev/null 2>&1; then
  $sudo systemctl stop warren-node >/dev/null 2>&1 || true
fi
if command -v launchctl >/dev/null 2>&1; then
  launchctl unload "$HOME/Library/LaunchAgents/com.warren.node.plist" >/dev/null 2>&1 || true
fi

# Install to /usr/local/bin when root or it is writable; otherwise to
# ~/.local/bin (no sudo prompt for a plain binary install).
if [ "$(id -u)" = "0" ] || [ -w /usr/local/bin ]; then
  dest="/usr/local/bin/warren"
  cp "$bin" "$dest"
else
  mkdir -p "$HOME/.local/bin"
  dest="$HOME/.local/bin/warren"
  cp "$bin" "$dest"
  echo "warren: installed to $dest (add \$HOME/.local/bin to PATH)." >&2
fi
chmod +x "$dest"
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
