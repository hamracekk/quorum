#!/bin/sh
# Install the watchdog as a launchd agent for the current user (macOS): a health check every
# 5 minutes for the listed slots and the recorder.
#   ./deploy/launchd/install-watchdog.sh [slots]     (default: crypto-spot; comma-separated)
# Undo with ./deploy/launchd/uninstall-watchdog.sh. Read-only: it never trades.
set -eu
slots=${1:-crypto-spot}
case $slots in
  *[!a-z0-9,-]* | "" | -* | ,*) echo "invalid slot list: $slots" >&2; exit 1 ;;
esac
repo=$(cd "$(dirname "$0")/../.." && pwd)
agents="$HOME/Library/LaunchAgents"
logs="$HOME/Library/Logs/quorum"
label="com.quorum.watchdog"
if [ ! -x "$repo/target/release/quorum" ]; then
  echo "build first: cargo build --release -p quorum-cli" >&2
  exit 1
fi
mkdir -p "$agents" "$logs"
plist="$agents/$label.plist"
sed -e "s|__REPO__|$repo|g" -e "s|__LOGS__|$logs|g" -e "s|__SLOTS__|$slots|g" \
  "$repo/deploy/launchd/$label.plist" > "$plist"
plutil -lint "$plist" >/dev/null
launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$plist"
echo "loaded $label for $slots (logs: $logs/watchdog.log)"
