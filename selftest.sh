#!/bin/bash
# Record yourself talking, then transcribe it immediately.
#
#   ./selftest.sh          90 seconds
#   ./selftest.sh 120      two minutes
#
# The point is question 2 in RESULTS.md: does Whisper survive real accented,
# code-switched speech? Everything tested so far has been TTS, which is far too
# clean to answer that. Your own voice is both real and the exact speaker the
# tool has to work for, and you know what you actually said — so you are the
# ground truth, no second person required.
#
# Talk the way you would in a meeting, not the way you would read aloud. Work
# in a few names, numbers and technical terms; those are what a note actually
# needs to get right, and they are where transcription fails expensively.
#
# Anything playing through your speakers or headphones lands in the system
# stream at the same time, so playing a conversation in the background turns
# this into a full two-stream rehearsal.
set -euo pipefail
cd "$(dirname "$0")"

DURATION="${1:-90}"
[ -d meetnote-capture.app ] || { echo "run ./build.sh first"; exit 1; }

OUT="$PWD/recordings/selftest-$(date +%H%M%S)"
mkdir -p "$OUT"

echo "recording ${DURATION}s — start talking"
echo
open -a "$PWD/meetnote-capture.app" --args --out "$OUT" --duration "$DURATION"
while pgrep -qf "meetnote-capture"; do sleep 1; done
sleep 1

grep -E "peak|silent|SILENT" "$OUT/capture.log" 2>/dev/null || true
echo
echo "transcribing…"
PY=./.venv/bin/python
[ -x "$PY" ] || PY=python3
"$PY" transcribe.py "$OUT" "${@:2}" 2>&1 | grep -vE "it/s\]" | grep -E "noise floor|segments kept|turns,"
echo
echo "──────── transcript ────────"
cat "$OUT/transcript.txt" 2>/dev/null || echo "(none produced)"
echo "────────────────────────────"
echo "$OUT"
