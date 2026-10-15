#!/bin/sh
# Remove the recorder's launchd agent. Recorded files stay in data/recorder/.
set -eu
label="com.quorum.recorder"
launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
rm -f "$HOME/Library/LaunchAgents/$label.plist"
echo "removed $label"
