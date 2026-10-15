#!/bin/sh
# Install one slot's paper session as a launchd agent for the current user (macOS).
#   ./deploy/launchd/install.sh [slot]        (default slot: crypto-spot)
# Check with `launchctl print gui/$(id -u)/com.quorum.paper.<slot>` or `quorum paper status`.
# Undo with ./deploy/launchd/uninstall.sh [slot]. Paper only: nothing here touches money or keys.
set -eu
slot=${1:-crypto-spot}
case $slot in
  *[!a-z0-9-]* | "" | -*) echo "invalid slot id: $slot" >&2; exit 1 ;;
esac
repo=$(cd "$(dirname "$0")/../.." && pwd)
agents="$HOME/Library/LaunchAgents"
logs="$HOME/Library/Logs/quorum"
label="com.quorum.paper.$slot"
if [ ! -x "$repo/target/release/quorum" ]; then
  echo "build first: cargo build --release -p quorum-cli" >&2
  exit 1
fi
if [ ! -f "$repo/config/slots/$slot.toml" ]; then
  echo "no slot config: config/slots/$slot.toml" >&2
  exit 1
fi
mkdir -p "$agents" "$logs"
plist="$agents/$label.plist"
sed -e "s|__REPO__|$repo|g" -e "s|__LOGS__|$logs|g" -e "s|__SLOT__|$slot|g" \
  "$repo/deploy/launchd/com.quorum.paper.plist" > "$plist"
plutil -lint "$plist" >/dev/null
launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$plist"
echo "loaded $label (logs: $logs/paper-$slot.log)"
