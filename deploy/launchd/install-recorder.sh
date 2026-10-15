#!/bin/sh
# Install the forward recorder as a launchd agent for the current user (macOS).
#   ./deploy/launchd/install-recorder.sh
# Undo with ./deploy/launchd/uninstall-recorder.sh. Public data only: no orders, no keys.
set -eu
repo=$(cd "$(dirname "$0")/../.." && pwd)
agents="$HOME/Library/LaunchAgents"
logs="$HOME/Library/Logs/quorum"
label="com.quorum.recorder"
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
echo "loaded $label (logs: $logs/recorder.log)"
