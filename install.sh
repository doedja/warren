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

# Termux (Android) has no systemd/launchd, so it gets its own setup path below:
# a Termux:Boot script + wake lock + battery-exemption prompt instead of a service.
termux=0
case "${PREFIX:-}" in *com.termux*) termux=1 ;; esac

fallback() {
  echo "warren: no prebuilt binary for $os/$arch." >&2
  echo "On Windows, use install.ps1. Otherwise build from source:" >&2
  echo "  cargo install --git https://github.com/$REPO warren" >&2
  exit 1
}

# Android/Termux node setup: automate the wake-lock + boot-restart + battery
# prompt that otherwise have to be done by hand. $dest is the installed binary.
setup_termux() {
  # CPU wake lock so Doze does not suspend the node (needs the termux-api pkg +
  # the Termux:API app). Best-effort; harmless if unavailable.
  pkg install -y termux-api >/dev/null 2>&1 || true
  termux-wake-lock >/dev/null 2>&1 || true

  # Boot script: re-acquire the lock and start the node on device boot. Runs only
  # once the Termux:Boot app is installed (one F-Droid install, opened once).
  bootdir="$HOME/.termux/boot"
  mkdir -p "$bootdir"
  {
    echo "#!$PREFIX/bin/sh"
    echo "termux-wake-lock 2>/dev/null || true"
    printf 'exec "%s" node run' "$dest"
    for a in "$@"; do printf ' "%s"' "$a"; done
    echo ""
  } >"$bootdir/start-warren.sh"
  chmod +x "$bootdir/start-warren.sh"

  # The one step Android requires a tap for: battery-optimization exemption.
  am start -a android.settings.REQUEST_IGNORE_BATTERY_OPTIMIZATIONS \
    -d package:com.termux >/dev/null 2>&1 || true

  echo "" >&2
  echo "Termux node set up. Two one-time taps so it survives sleep + reboot:" >&2
  echo "  1. Install Termux:Boot and Termux:API from F-Droid, open each once." >&2
  echo "  2. Accept the battery-optimization dialog that just opened." >&2
  echo "Starting the node now (logs: ~/warren-node.log)..." >&2
  nohup "$dest" node run "$@" >"$HOME/warren-node.log" 2>&1 &
  echo "warren: node running in the background (pid $!)." >&2
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

# Release assets are version-stamped (warren-<tag>-<os>-<arch>.tar.gz), so resolve
# the latest tag first instead of using the /latest/download/ shortcut.
fetch() { if command -v curl >/dev/null 2>&1; then curl -fsSL "$1"; else wget -qO- "$1"; fi; }
tag="$(fetch "https://api.github.com/repos/$REPO/releases/latest" | grep -m1 '"tag_name"' | cut -d'"' -f4)"
if [ -z "$tag" ]; then
  echo "warren: could not resolve the latest release tag." >&2
  exit 1
fi
url="https://github.com/$REPO/releases/download/$tag/warren-$tag-$asset.tar.gz"
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

# Verify the download against the sha256 published next to it before extracting
# or running anything. Auto-update re-runs this script, so self-updates are
# covered too. Fail closed; WARREN_SKIP_VERIFY=1 bypasses (not recommended).
if [ "${WARREN_SKIP_VERIFY:-0}" != "1" ]; then
  expected="$(fetch "$url.sha256" 2>/dev/null | awk '{print $1}' | head -n1)"
  if [ -z "$expected" ]; then
    echo "warren: could not fetch checksum ($url.sha256)." >&2
    echo "        set WARREN_SKIP_VERIFY=1 to bypass (not recommended)." >&2
    exit 1
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$tmp/warren.tar.gz" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$tmp/warren.tar.gz" | awk '{print $1}')"
  elif command -v openssl >/dev/null 2>&1; then
    actual="$(openssl dgst -sha256 "$tmp/warren.tar.gz" | awk '{print $NF}')"
  else
    echo "warren: no sha256 tool (sha256sum/shasum/openssl) to verify the download." >&2
    echo "        set WARREN_SKIP_VERIFY=1 to bypass (not recommended)." >&2
    exit 1
  fi
  if [ "$expected" != "$actual" ]; then
    echo "warren: checksum mismatch (expected $expected, got $actual). Aborting." >&2
    exit 1
  fi
  echo "warren: checksum verified (sha256)." >&2
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

if [ "$#" -gt 0 ] && [ "$termux" = 1 ]; then
  # Android: no systemd/launchd; wire up wake-lock + boot-restart + start now.
  setup_termux "$@"
elif [ "$#" -gt 0 ]; then
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
