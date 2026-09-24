#!/usr/bin/env python3
"""make_fixture.py — synthesise a two-stream recording for testing the pipeline.

    ./make_fixture.py fixtures/demo

Renders a scripted dialogue with macOS TTS into mic.wav + system.wav + capture.json,
laid out exactly the way ./capture would write them, so transcribe.py and
enhance.py can be exercised without a real meeting or audio-capture permission.

This validates plumbing, the timestamp merge and prompt behaviour. It does NOT
answer the question of whether Whisper survives real accented, code-switched
audio — TTS is far too clean for that. Only a real call answers it.
"""

import argparse
import json
import shutil
import subprocess
import sys
import tempfile
import wave
from array import array
from pathlib import Path

SAMPLE_RATE = 48000
ME, THEM = "mic", "system"

# The mic stream is written 0.2 s late on purpose, so the offset shifting in
# transcribe.py is actually exercised rather than being a no-op.
MIC_OFFSET = 0.2

# (stream, voice, global start seconds, text)
# Deliberate properties, used as assertions about behaviour further down:
#   - Hinglish code-switching throughout
#   - names, numbers and a version string that must survive transcription
#   - the on-call rotation topic gets ~25 s of airtime but no note fragment,
#     so it must NOT appear in the enhanced note
DIALOGUE = [
    (THEM, "Tara", 0.5, "Okay, let's start. Priya, can you give us the update on the migration?"),
    (ME, "Rishi", 5.0, "Haan, so the Postgres migration is about forty percent done. We moved the events table last week."),
    (ME, "Rishi", 12.5, "The problem is the webhook retries. We are seeing duplicate deliveries because idempotency keys are not being set properly."),
    (THEM, "Tara", 21.0, "Thoda detail do na. How many duplicates are we talking about?"),
    (ME, "Rishi", 26.0, "Roughly twelve thousand over the last week. Matlab it is not huge, but customers are noticing."),
    (THEM, "Aman", 33.5, "Theek hai. I think we should ship the idempotency fix before the version two point four release on Friday."),
    (THEM, "Tara", 42.0, "Can we do that in three weeks, or is Friday realistic?"),
    (ME, "Rishi", 48.0, "Friday is realistic for the fix. The full migration will take three more weeks."),
    (THEM, "Aman", 55.0, "Okay. Now let's talk about the on-call rotation. Rohan has been covering nights for a month."),
    (THEM, "Tara", 63.0, "Yes, we need to rebalance that. Meera can take Tuesdays and Thursdays starting next week."),
    (THEM, "Aman", 71.0, "And we should document the escalation path properly, it is still tribal knowledge."),
    (ME, "Rishi", 79.0, "I will own the idempotency fix and have it in staging by Wednesday."),
    (THEM, "Tara", 86.0, "Perfect. Let's keep the Kafka consumer lag dashboard on the agenda for next week."),
]

# Sparse, lowercase, abbreviated — the way someone actually types mid-call.
# The last fragment is unsupported by the transcript on purpose.
NOTES = """- postgres migration 40%
- webhook dupes - idempotency keys
- friday v2.4 ship?
- my action: fix in staging by wed
- ask design team about the new onboarding flow
"""


def render(voice, text, path):
    subprocess.run(
        ["say", "-v", voice, "-o", str(path), "--data-format=LEI16@48000", text],
        check=True,
        capture_output=True,
    )
    with wave.open(str(path), "rb") as handle:
        frames = handle.readframes(handle.getnframes())
    samples = array("h")
    samples.frombytes(frames)
    return samples


def write_wave(path, samples, channels):
    with wave.open(str(path), "wb") as handle:
        handle.setnchannels(channels)
        handle.setsampwidth(2)
        handle.setframerate(SAMPLE_RATE)
        if channels == 2:
            stereo = array("h", [0]) * (len(samples) * 2)
            stereo[0::2] = samples
            stereo[1::2] = samples
            handle.writeframes(stereo.tobytes())
        else:
            handle.writeframes(samples.tobytes())


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("output", type=Path, nargs="?", default=Path("fixtures/demo"))
    args = parser.parse_args()

    if shutil.which("say") is None:
        sys.exit("`say` not found — this fixture generator is macOS only")

    args.output.mkdir(parents=True, exist_ok=True)
    total = int((max(start for _, _, start, _ in DIALOGUE) + 20) * SAMPLE_RATE)
    tracks = {ME: array("h", [0]) * total, THEM: array("h", [0]) * total}

    # macOS ships voice metadata before voice data. `say` will happily render
    # with a fallback the first time a voice is used — different timing, no
    # error, exit status 0 — and fetch the real voice in the background. That
    # silently produces a fixture whose audio does not match its own layout.
    # Render each voice once and throw it away so the real one is installed.
    with tempfile.TemporaryDirectory() as tmp:
        for voice in sorted({voice for _, voice, _, _ in DIALOGUE}):
            render(voice, "warm up", Path(tmp) / f"warm-{voice}.wav")

    # Ground truth for the diarisation benchmark: who spoke, on which stream,
    # between which seconds. Written from the real rendered durations rather
    # than the intended ones, so it describes the audio that actually exists.
    truth = []

    with tempfile.TemporaryDirectory() as tmp:
        for index, (stream, voice, start, text) in enumerate(DIALOGUE):
            samples = render(voice, text, Path(tmp) / f"{index}.wav")
            # Each stream carries its own local clock; the mic one runs late.
            local = start - (MIC_OFFSET if stream == ME else 0.0)
            at = int(local * SAMPLE_RATE)
            track = tracks[stream]
            for i, sample in enumerate(samples):
                if at + i < total:
                    track[at + i] = sample
            truth.append({
                "stream": stream,
                "speaker": voice,
                "start_secs": at / SAMPLE_RATE,
                "end_secs": (at + len(samples)) / SAMPLE_RATE,
            })
            print(f"  {stream:6s} {voice:6s} {start:6.1f}s  {text[:60]}")

    write_wave(args.output / "mic.wav", tracks[ME], channels=1)
    write_wave(args.output / "system.wav", tracks[THEM], channels=2)

    meta = {
        "started_at": "2026-08-14T18:00:00Z",
        "streams": [
            {
                "label": "mic", "speaker": "ME", "path": "mic.wav",
                "sample_rate": SAMPLE_RATE, "channels": 1,
                "frames": total, "duration_seconds": total / SAMPLE_RATE,
                "offset_seconds": MIC_OFFSET,
            },
            {
                "label": "system", "speaker": "THEM", "path": "system.wav",
                "sample_rate": SAMPLE_RATE, "channels": 2,
                "frames": total, "duration_seconds": total / SAMPLE_RATE,
                "offset_seconds": 0.0,
            },
        ],
    }
    (args.output / "capture.json").write_text(json.dumps(meta, indent=2))
    (args.output / "notes.md").write_text(NOTES)
    (args.output / "truth.json").write_text(json.dumps(truth, indent=2))
    speakers = sorted({t["speaker"] for t in truth if t["stream"] == THEM})
    print(f"\n  {total / SAMPLE_RATE:.0f}s fixture → {args.output}")
    print(f"  far-end speakers for the diarisation benchmark: {', '.join(speakers)}")


if __name__ == "__main__":
    main()
