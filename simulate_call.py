#!/usr/bin/env python3
"""simulate_call.py — approximate what a meeting codec does to captured audio.

    ./simulate_call.py recordings/<ts>            → recordings/<ts>-call/

Zoom, Meet and Teams all carry voice as Opus at roughly 16-32 kbps wideband.
Anything you capture from a YouTube video is far cleaner than that, so a
transcript built from it flatters Whisper. This runs the audio through an Opus
round-trip so you can measure how much of the transcript survives call-grade
compression.

Only the system stream is degraded, which is the honest model: in a real call
your own microphone is recorded locally at full quality, while the far end
reaches you already encoded and decoded once.

Stdlib only.
"""

import argparse
import json
import shutil
import subprocess
import sys
from pathlib import Path

# Roughly what a meeting client allocates to a voice stream.
DEFAULT_BITRATE = "24k"


def degrade(source, destination, bitrate):
    """Opus round-trip at meeting bitrate, back out as a wav."""
    intermediate = destination.with_suffix(".opus")
    subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", str(source),
         "-c:a", "libopus", "-b:a", bitrate, "-ar", "16000", "-ac", "1",
         "-application", "voip", str(intermediate)],
        check=True,
    )
    subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", str(intermediate),
         "-ar", "16000", "-ac", "1", str(destination)],
        check=True,
    )
    intermediate.unlink()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("recording", type=Path)
    parser.add_argument("--out", type=Path, help="default: <recording>-call")
    parser.add_argument("--bitrate", default=DEFAULT_BITRATE, help=f"opus bitrate (default {DEFAULT_BITRATE})")
    parser.add_argument("--both", action="store_true",
                        help="also degrade the mic stream (not what a real call does)")
    args = parser.parse_args()

    if shutil.which("ffmpeg") is None:
        sys.exit("ffmpeg not found")
    meta_path = args.recording / "capture.json"
    if not meta_path.exists():
        sys.exit(f"no capture.json in {args.recording}")
    meta = json.loads(meta_path.read_text())

    out = args.out or args.recording.parent / (args.recording.name + "-call")
    out.mkdir(parents=True, exist_ok=True)

    for stream in meta["streams"]:
        source = args.recording / stream["path"]
        if not source.exists():
            continue
        destination = out / stream["path"]
        if stream["label"] == "system" or args.both:
            degrade(source, destination, args.bitrate)
            stream["sample_rate"] = 16000
            stream["channels"] = 1
            print(f"  {stream['label']:7s} → opus {args.bitrate} 16 kHz mono")
        else:
            shutil.copy2(source, destination)
            print(f"  {stream['label']:7s} → copied unchanged (local capture is not degraded in a call)")

    (out / "capture.json").write_text(json.dumps(meta, indent=2))
    notes = args.recording / "notes.md"
    if notes.exists():
        shutil.copy2(notes, out / "notes.md")
    print(f"\n  {out}")
    print("  now transcribe both and diff the results")


if __name__ == "__main__":
    main()
