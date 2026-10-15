#!/bin/sh
# Remove the launchd agent install.sh added for a slot. The session stays resumable:
# `quorum paper run --slot <slot>` (or install.sh again) picks it up where it stopped.
#   ./deploy/launchd/uninstall.sh [slot]      (default slot: crypto-spot)
set -eu
slot=${1:-crypto-spot}
case $slot in
  *[!a-z0-9-]* | "" | -*) echo "invalid slot id: $slot" >&2; exit 1 ;;
esac
label="com.quorum.paper.$slot"
launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
rm -f "$HOME/Library/LaunchAgents/$label.plist"
echo "removed $label"
