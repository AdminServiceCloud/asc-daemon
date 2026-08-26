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
#   --silent        accept the defaults and ask nothing; installs Docker,
#                   because container apps are the point of a managed node
#   --no-docker     never install Docker, not even with --silent
#   --direct        expose the daemon API to the network over TLS, so the
#                   platform can reach it without an SSH tunnel
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
WANT_DOCKER=1
DIRECT=0

fail() { echo "error: $*" >&2; exit 1; }

# A while loop, not a for loop: --token takes a separate value.
while [ "$#" -gt 0 ]; do
    case "$1" in
        --silent)    SILENT=1 ;;
        --no-docker) WANT_DOCKER=0 ;;
        --direct)    DIRECT=1 ;;
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

# ── Docker: container apps need it ─────────────────────────
# The script usually arrives via `curl | bash`, so stdin is the script itself —
# interactive answers are read from /dev/tty when there is one.
#
# An unattended run installs Docker instead of only warning about it: --silent
# is what the platform uses to provision a node, and a node that cannot run
# container apps is not a working node. --no-docker opts out.
install_docker() {
    echo "Installing Docker (get.docker.com)..."
    if curl -fsSL --proto '=https' --tlsv1.2 https://get.docker.com | sh; then
        systemctl enable --now docker >/dev/null 2>&1 || true
        echo "Docker installed"
        return 0
    fi
    return 1
}

if [ "$WANT_DOCKER" -eq 0 ]; then
    command -v docker >/dev/null 2>&1         || echo "Skipping Docker (--no-docker) — container apps will not run."
elif ! command -v docker >/dev/null 2>&1; then
    if [ "$SILENT" -eq 0 ] && [ -r /dev/tty ] && [ -w /dev/tty ]; then
        printf "Docker is not installed. Container apps (asc install <pkg>) need it.
Install Docker now via get.docker.com? [Y/n] " > /dev/tty
        read -r answer < /dev/tty || answer=""
        case "$answer" in
            n|N|no|NO|н|нет|Н|Нет)
                echo "Skipping Docker. Install it later with: curl -fsSL https://get.docker.com | sh"
                ;;
            *)
                install_docker || fail "Docker installation failed"
                ;;
        esac
    else
        # Never fatal: the daemon manages native apps without Docker, and
        # aborting the install over it would leave the node unmanaged.
        install_docker || {
            echo "warning: Docker installation failed — container apps will not run." >&2
            echo "         Retry with: curl -fsSL https://get.docker.com | sh" >&2
        }
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
[ "$DIRECT" -eq 1 ] && set -- "$@" --direct
[ -n "$PLATFORM_URL" ] && set -- "$@" --url "$PLATFORM_URL"
exec "${INSTALL_DIR}/asc-updater" "$@"
