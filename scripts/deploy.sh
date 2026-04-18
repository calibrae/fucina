#!/usr/bin/env bash
# Deploy fucina to a remote macOS host.
#
# Builds (if needed) the signed+notarized+stapled pkg, scps it, then runs
# install.sh on the remote host over SSH.
#
# Usage:
#   ./scripts/deploy.sh HOST [--token TOKEN] [--name NAME] [--labels LABELS]
#                            [--instance URL] [--skip-build]
#
# Env:
#   SSH_USER      override SSH username (default: current user)
#
# Requires:
#   - make pkg works locally (Developer ID Application + Installer certs,
#     FUCINA_NOTARY keychain profile)
#   - `gitea actions generate-runner-token` accessible if --token is not given

set -euo pipefail

HOST=""
TOKEN=""
NAME=""
LABELS=""
INSTANCE=""
SKIP_BUILD=0
SSH_USER="${SSH_USER:-}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --token) TOKEN="$2"; shift 2;;
        --name) NAME="$2"; shift 2;;
        --labels) LABELS="$2"; shift 2;;
        --instance) INSTANCE="$2"; shift 2;;
        --skip-build) SKIP_BUILD=1; shift;;
        -*) echo "unknown flag: $1" >&2; exit 1;;
        *) [ -z "$HOST" ] && HOST="$1" || { echo "extra arg: $1" >&2; exit 1; }; shift;;
    esac
done

[ -n "$HOST" ] || { echo "usage: $0 HOST [--token TOKEN] [--name NAME] [--labels LABELS] [--instance URL] [--skip-build]" >&2; exit 1; }

cd "$(dirname "$0")/.."

if [ $SKIP_BUILD -eq 0 ]; then
    echo "==> building signed+notarized+stapled pkg"
    make pkg
fi

PKG=$(ls -t target/fucina-*.pkg 2>/dev/null | head -1)
[ -n "$PKG" ] || { echo "no pkg found in target/ — run 'make pkg' first" >&2; exit 1; }
echo "==> pkg: $PKG"

SSH_TARGET="${SSH_USER:+$SSH_USER@}$HOST"

if [ -z "$TOKEN" ]; then
    echo "==> fetching registration token from git.calii.net"
    TOKEN=$(ssh git.calii.lan "sudo -u git /usr/local/bin/gitea actions generate-runner-token -c /etc/gitea/app.ini" | tr -d '[:space:]')
fi
[ -n "$TOKEN" ] || { echo "no token available" >&2; exit 1; }

PKG_BASENAME=$(basename "$PKG")
echo "==> copying $PKG_BASENAME to $SSH_TARGET"
scp -q "$PKG" "$SSH_TARGET:/tmp/$PKG_BASENAME"
scp -q scripts/install.sh "$SSH_TARGET:/tmp/fucina-install.sh"

echo "==> running install.sh on $SSH_TARGET"
# -t for TTY so sudo can prompt for password
ssh -t "$SSH_TARGET" "chmod +x /tmp/fucina-install.sh && /tmp/fucina-install.sh /tmp/$PKG_BASENAME '$TOKEN' '${INSTANCE:-https://git.calii.net}' '${NAME:-}' '${LABELS:-}'"

echo "==> cleanup"
ssh "$SSH_TARGET" "rm -f /tmp/$PKG_BASENAME /tmp/fucina-install.sh"

echo ""
echo "==> done — remember to approve the Local Network prompt in $SSH_TARGET's GUI session"
