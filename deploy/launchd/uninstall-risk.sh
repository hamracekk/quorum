#!/bin/sh
# Remove the risk process's launchd agent. Its state (halts, loss references) stays in
# data/risk/. Paper slots then refuse every order that adds risk.
set -eu
label="com.quorum.risk"
launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
rm -f "$HOME/Library/LaunchAgents/$label.plist"
echo "removed $label"
