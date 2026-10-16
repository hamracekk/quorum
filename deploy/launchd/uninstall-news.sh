#!/bin/sh
# Remove the news service's launchd agent. Stored news and readings stay in the journal..
set -eu
label="com.quorum.news"
launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
rm -f "$HOME/Library/LaunchAgents/$label.plist"
echo "removed $label"
