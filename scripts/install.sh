#!/usr/bin/env bash
# Install fucina on a local macOS host as a per-user LaunchAgent.
#
# Runs on the target machine. Requires the signed+notarized+stapled pkg
# to already be present (either passed as arg or via stdin via `deploy.sh`).
#
# Usage:
#   sudo ./install.sh PKG TOKEN [INSTANCE] [NAME] [LABELS]
#
# Args:
#   PKG       path to fucina-X.Y.Z.pkg (required)
#   TOKEN     Gitea registration token (required — generate via
#             `sudo -u git gitea actions generate-runner-token -c /etc/gitea/app.ini`)
#   INSTANCE  Gitea URL (default: https://git.calii.net)
#   NAME      runner name (default: <hostname -s>-rs)
#   LABELS    comma-sep labels (default: self-hosted:host,macos-arm64:host)
#
# After install:
#   - Binary at /usr/local/bin/fucina (from pkg)
#   - Config + credentials at ~/gitea-runner-rs/
#   - LaunchAgent net.calii.fucina bootstrapped into the user session
#   - On first poll to the Gitea LAN address, macOS prompts for Local Network
#     access — accept it in the GUI session.

set -euo pipefail

PKG="${1:?pkg path required}"
TOKEN="${2:?registration token required}"
INSTANCE="${3:-https://git.calii.net}"
NAME="${4:-$(hostname -s)-rs}"
LABELS="${5:-self-hosted:host,macos-arm64:host}"

RUNNER_HOME="$HOME/gitea-runner-rs"
PLIST="$HOME/Library/LaunchAgents/net.calii.fucina.plist"
LABEL="net.calii.fucina"

[ -f "$PKG" ] || { echo "pkg not found: $PKG" >&2; exit 1; }
[ "$EUID" -eq 0 ] && { echo "don't run as root — sudo is used only where needed" >&2; exit 1; }

echo "==> installing $PKG to /usr/local/bin/fucina"
sudo installer -pkg "$PKG" -target /

echo "==> preparing $RUNNER_HOME"
mkdir -p "$RUNNER_HOME"
cat > "$RUNNER_HOME/config.yaml" <<EOF
instance: $INSTANCE
name: $NAME
labels:
$(echo "$LABELS" | tr ',' '\n' | sed 's/^/  - /')
capacity: 1
fetch_interval: 2
timeout: 10800
work_dir: /tmp/fucina
EOF

echo "==> registering runner"
cd "$RUNNER_HOME"
FUCINA_BIN=/Applications/Fucina.app/Contents/MacOS/fucina
[ -x "$FUCINA_BIN" ] || FUCINA_BIN=/usr/local/bin/fucina
"$FUCINA_BIN" --config config.yaml register --token "$TOKEN"

echo "==> writing LaunchAgent plist $PLIST"
mkdir -p "$(dirname "$PLIST")"
cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>$LABEL</string>
    <key>ProgramArguments</key>
    <array>
        <string>$FUCINA_BIN</string>
        <string>--config</string><string>$RUNNER_HOME/config.yaml</string>
        <string>daemon</string>
    </array>
    <key>WorkingDirectory</key><string>$RUNNER_HOME</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key><string>/usr/bin:/bin:/usr/sbin:/sbin:$HOME/.cargo/bin:/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin</string>
    </dict>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>StandardOutPath</key><string>$RUNNER_HOME/runner.log</string>
    <key>StandardErrorPath</key><string>$RUNNER_HOME/runner.log</string>
</dict>
</plist>
EOF

echo "==> bootstrapping LaunchAgent into GUI session"
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
: > "$RUNNER_HOME/runner.log"
launchctl bootstrap "gui/$(id -u)" "$PLIST"

sleep 3
if pgrep -fl fucina | grep -q "$RUNNER_HOME/config.yaml"; then
    echo "==> fucina is running"
else
    echo "!! fucina did not start — check $RUNNER_HOME/runner.log"
    tail -20 "$RUNNER_HOME/runner.log" || true
    exit 1
fi

echo ""
echo "==> next steps"
echo "  1. Log into $USER's GUI session on this host (Screen Sharing / VNC / console)"
echo "  2. Watch for the macOS 'Local Network' permission prompt"
echo "  3. Click Allow — fucina will then reach the Gitea LAN instance"
echo "  4. Optionally: System Settings → Privacy & Security → Full Disk Access → add /usr/local/bin/fucina"
echo ""
echo "==> to re-deploy after a new release:"
echo "  sudo installer -pkg fucina-X.Y.Z.pkg -target /"
echo "  launchctl kickstart -k gui/\$(id -u)/$LABEL"
