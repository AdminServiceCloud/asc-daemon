#!/usr/bin/env bash
# 🦀 asc-daemon installer.
# Downloads the asc-updater binary from the latest GitHub release; the updater
# then installs and manages the daemon itself (channels, auto-updates, rollback).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/AdminServiceCloud/asc-daemon/main/install.sh | sudo bash
#   curl -fsSL https://raw.githubusercontent.com/AdminServiceCloud/asc-daemon/main/install.sh | sudo bash -s -- --silent
set -euo pipefail

REPO="AdminServiceCloud/asc-daemon"
INSTALL_DIR="/usr/local/bin"
SILENT=0

for arg in "$@"; do
    case "$arg" in
        --silent) SILENT=1 ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

fail() { echo "error: $*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || fail "this installer requires root (run with sudo)"
command -v curl >/dev/null 2>&1 || fail "curl is required"

# ── OS check: Debian/Ubuntu are supported, other distributions best-effort ──
[ "$(uname -s)" = "Linux" ] || fail "only Linux is supported for now (macOS is on the roadmap)"
if [ -r /etc/os-release ]; then
    . /etc/os-release
    case "${ID:-}:${ID_LIKE:-}" in
        debian:*|ubuntu:*|*:*debian*) ;;
        *) echo "warning: untested distribution '${ID:-unknown}' — Debian and Ubuntu are the supported targets" >&2 ;;
    esac
fi

# ── Architecture → Rust target triple (as published in releases) ────────────
case "$(uname -m)" in
    x86_64)          TARGET="x86_64-unknown-linux-gnu" ;;
    aarch64|arm64)   TARGET="aarch64-unknown-linux-gnu" ;;
    armv7l)          TARGET="armv7-unknown-linux-gnueabihf" ;;
    *) fail "unsupported architecture: $(uname -m)" ;;
esac

ASSET="asc-updater-${TARGET}"
URL="https://github.com/${REPO}/releases/latest/download/${ASSET}"
TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT

echo "Downloading ${ASSET} (latest release)..."
curl -fSL --proto '=https' --tlsv1.2 -o "$TMP" "$URL" \
    || fail "download failed: $URL (no releases published yet?)"

install -m 755 "$TMP" "${INSTALL_DIR}/asc-updater"
echo "Installed ${INSTALL_DIR}/asc-updater"

# The updater takes over: shows default settings and asks to accept or change
# them; --silent installs everything with defaults, no questions asked.
if [ "$SILENT" -eq 1 ]; then
    exec "${INSTALL_DIR}/asc-updater" install --silent
else
    exec "${INSTALL_DIR}/asc-updater" install
fi
