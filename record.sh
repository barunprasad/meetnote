#!/bin/bash
# Launch capture as an app rather than as a child of your terminal.
#
#   ./record.sh          record until you stop it (default 3600s cap)
#   ./record.sh 30       record for 30 seconds
#
# `open` hands the launch to launchd, so the app becomes responsible for its own
# permissions and macOS shows the consent prompt with our usage strings, instead
# of silently auto-denying a CLI child of the terminal.
set -euo pipefail
cd "$(dirname "$0")"

APP="$PWD/meetnote-capture.app"
[ -d "$APP" ] || { echo "no $APP — run ./build.sh first"; exit 1; }

DURATION="${1:-3600}"
OUT="$PWD/recordings/$(date +%Y-%m-%d-%H%M%S)"
mkdir -p "$OUT"

open -a "$APP" --args --out "$OUT" --duration "$DURATION"
echo "recording up to ${DURATION}s → $OUT"
echo "(approve the permission prompt if one appears)"
echo

# Wait for it to finish, then show what it logged.
while pgrep -qf "meetnote-capture"; do sleep 1; done
sleep 1
if [ -f "$OUT/capture.log" ]; then
    cat "$OUT/capture.log"
else
    echo "no capture.log written — the app may not have launched at all"
fi
