#!/bin/sh
# Remove the watchdog's launchd agent.
set -eu
label="com.quorum.watchdog"
launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
rm -f "$HOME/Library/LaunchAgents/$label.plist"
echo "removed $label"
