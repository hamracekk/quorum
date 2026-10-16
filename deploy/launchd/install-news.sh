#!/bin/sh
# Install the news service (`quorum news run`) as a launchd agent for the current user (macOS).
#   ./deploy/launchd/install-news.sh
# Undo with ./deploy/launchd/uninstall-news.sh. Needs the Anthropic key (quorum secrets set anthropic) to read; never trades.
set -eu
repo=$(cd "$(dirname "$0")/../.." && pwd)
agents="$HOME/Library/LaunchAgents"
logs="$HOME/Library/Logs/quorum"
label="com.quorum.news"
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
echo "loaded $label (logs: $logs/news.log)"
