#!/bin/sh
# Install the risk process (`quorum risk run`) as a launchd agent for the current user (macOS).
#   ./deploy/launchd/install-risk.sh
# Undo with ./deploy/launchd/uninstall-risk.sh. Install it before the paper slots: without it
# they refuse every order that adds risk.
set -eu
repo=$(cd "$(dirname "$0")/../.." && pwd)
agents="$HOME/Library/LaunchAgents"
logs="$HOME/Library/Logs/quorum"
label="com.quorum.risk"
if [ ! -x "$repo/target/release/quorum" ]; then
  echo "build first: cargo build --release -p quorum-cli" >&2
  exit 1
fi
mkdir -p "$agents" "$logs"
plist="$agents/$label.plist"
sed -e "s|__REPO__|$repo|g" -e "s|__LOGS__|$logs|g" \
  "$repo/deploy/launchd/$label.plist" > "$plist"
plutil -lint "$plist" >/dev/null
launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$plist"
echo "loaded $label (logs: $logs/risk.log)"
