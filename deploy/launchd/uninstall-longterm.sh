#!/bin/sh
# Remove the long-term book's launchd agent. Its session, lots and reviews stay in data/longterm/;
# the next `quorum longterm paper` resumes the session.
set -eu
label="com.quorum.longterm"
launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
rm -f "$HOME/Library/LaunchAgents/$label.plist"
echo "removed $label"
