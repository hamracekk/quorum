#!/bin/sh
# Install the long-term paper book (`quorum longterm paper`, every 6 hours) as a launchd agent (macOS).
#   ./deploy/launchd/install-longterm.sh
# Undo with ./deploy/launchd/uninstall-longterm.sh. Needs the risk process (install-risk.sh), a
# Tiingo key and a [longterm] section in config/envelope.toml.
set -eu
repo=$(cd "$(dirname "$0")/../.." && pwd)
agents="$HOME/Library/LaunchAgents"
logs="$HOME/Library/Logs/quorum"
label="com.quorum.longterm"
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
echo "loaded $label (logs: $logs/longterm.log)"
