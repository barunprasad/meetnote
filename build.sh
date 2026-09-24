#!/bin/bash
# Builds capture as a real .app bundle, not a bare CLI binary.
#
# This is not cosmetic. macOS attributes a CLI tool's permission requests to
# whatever launched it (the "responsible process"), so a bare binary cannot own
# an audio-capture grant — and if the parent terminal will not surface the
# prompt, TCC silently auto-denies and you get a recording full of zeros with no
# error anywhere. Launching a signed .app via `open` makes it responsible for
# itself, so it gets its own TCC identity and its own prompt.
set -euo pipefail
cd "$(dirname "$0")"

APP="meetnote-capture.app"
BINARY="meetnote-capture"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"

swiftc -O capture.swift -o "$APP/Contents/MacOS/$BINARY" \
    -framework AVFoundation \
    -framework AudioToolbox \
    -framework CoreAudio \
    -Xlinker -sectcreate \
    -Xlinker __TEXT \
    -Xlinker __info_plist \
    -Xlinker Info.plist

cp Info.plist "$APP/Contents/Info.plist"

# Sign the whole bundle. A stable identifier keeps the TCC grant across
# rebuilds; without it every build looks like a different app and re-prompts.
codesign --force --deep --sign - --identifier local.meetnote.capture "$APP"

# Convenience symlink so ./capture still works for the direct-run path.
ln -sf "$APP/Contents/MacOS/$BINARY" capture

echo "built $APP (and ./capture symlink)"
echo
echo "If ./capture gets no permission prompt, launch it as an app instead:"
echo "    ./record.sh 30"
