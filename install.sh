#!/usr/bin/env bash
# 🦀 asc-daemon installer.
# Downloads the asc-updater binary from the latest GitHub release; the updater
# then installs and manages the daemon itself (channels, auto-updates, rollback).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/AdminServiceCloud/asc-daemon/main/install.sh | sudo bash
#   curl -fsSL https://raw.githubusercontent.com/AdminServiceCloud/asc-daemon/main/install.sh | sudo bash -s -- --silent
#
# Options:
#   --silent        accept the defaults and ask nothing
#   --token <TOKEN> one-time registration token from the platform; the node is
#                   bound to the organization that issued it
#   --url <URL>     platform base URL (default https://adminservice.cloud)
#
# Both --opt value and --opt=value are accepted.
set -euo pipefail

REPO="AdminServiceCloud/asc-daemon"
INSTALL_DIR="/usr/local/bin"
SILENT=0
TOKEN=""
PLATFORM_URL=""

fail() { echo "error: $*" >&2; exit 1; }

# A while loop, not a for loop: --token takes a separate value.
while [ "$#" -gt 0 ]; do
    case "$1" in
        --silent)  SILENT=1 ;;
        --token)   shift; TOKEN="${1:-}" ;;
        --token=*) TOKEN="${1#--token=}" ;;
        --url)     shift; PLATFORM_URL="${1:-}" ;;
        --url=*)   PLATFORM_URL="${1#--url=}" ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

case "$TOKEN" in
    "") ;;
    *[!A-Za-z0-9_-]*) fail "--token may only contain letters, digits, '-' and '_'" ;;
esac
case "$PLATFORM_URL" in
    ""|https://*|http://*) ;;
    *) fail "--url must start with https:// or http://" ;;
esac
[ -n "$PLATFORM_URL" ] && [ -z "$TOKEN" ] && fail "--url makes sense only together with --token"

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

# ── Docker: container apps need it; offer to install when missing ───────────
# The script usually arrives via `curl | bash`, so stdin is the script itself —
# interactive answers are read from /dev/tty when there is one.
if ! command -v docker >/dev/null 2>&1; then
    if [ "$SILENT" -eq 0 ] && [ -r /dev/tty ] && [ -w /dev/tty ]; then
        printf "Docker is not installed. Container apps (asc install <pkg>) need it.\nInstall Docker now via get.docker.com? [y/N] " > /dev/tty
        read -r answer < /dev/tty || answer=""
        case "$answer" in
            y|Y|yes|YES|д|да|Д|Да)
                echo "Installing Docker (get.docker.com)..."
                curl -fsSL --proto '=https' --tlsv1.2 https://get.docker.com | sh \
                    || fail "Docker installation failed"
                systemctl enable --now docker >/dev/null 2>&1 || true
                echo "Docker installed"
                ;;
            *)
                echo "Skipping Docker. Install it later with: curl -fsSL https://get.docker.com | sh"
                ;;
        esac
    else
        echo "warning: Docker is not installed — container apps will not run." >&2
        echo "         Install it with: curl -fsSL https://get.docker.com | sh" >&2
    fi
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
set -- install
[ "$SILENT" -eq 1 ] && set -- "$@" --silent
[ -n "$TOKEN" ] && set -- "$@" --token "$TOKEN"
[ -n "$PLATFORM_URL" ] && set -- "$@" --url "$PLATFORM_URL"
exec "${INSTALL_DIR}/asc-updater" "$@"
