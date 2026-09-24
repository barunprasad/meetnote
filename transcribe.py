#!/usr/bin/env python3
"""transcribe.py — transcribe both capture streams and merge them by timestamp.

    ./transcribe.py recordings/2026-08-14-234500

Reads capture.json + mic.wav + system.wav, runs the chosen STT engine on each
stream separately, shifts each stream onto a shared timeline using the host-clock
offsets recorded at capture time, and writes a single labelled transcript.

Speaker attribution comes from which file the audio was in — mic is ME,
the system tap is THEM. No diarisation model involved.
"""

import argparse
import difflib
import json
import re
import subprocess
import sys
from pathlib import Path

import numpy as np

import engines

# Whisper emits these into silence. Each stream in a two-stream capture is
# silent whenever the other person is talking, so this is not a rare edge case
# here — it is most of the audio.
HALLUCINATION_RE = re.compile(
    r"^\W*(thank you|thanks for watching|thanks|please subscribe|subscribe|"
    r"bye|okay|ok|you|mbc \S+|amara\.org|www\.\S+|\[.*\]|\(.*\))\W*$",
    re.IGNORECASE,
)


def log(message):
    print(message, file=sys.stderr, flush=True)


def load_audio(path, sample_rate=16000):
    """16 kHz mono float32 via ffmpeg.

    Deliberately not routed through any engine's loader — the energy analysis
    below has to work even when the chosen engine is a cloud API and no local
    STT package is installed at all.
    """
    raw = subprocess.run(
        ["ffmpeg", "-nostdin", "-loglevel", "error", "-i", str(path),
         "-f", "s16le", "-ac", "1", "-ar", str(sample_rate), "-"],
        capture_output=True, check=True,
    ).stdout
    return np.frombuffer(raw, np.int16).astype(np.float32) / 32768.0


FRAME_MS = 20
# ≈ -50 dBFS. Below this nothing is speech worth keeping, whatever Whisper says.
ABSOLUTE_FLOOR = 3.2e-3


def frame_energy(samples, sample_rate=16000):
    """RMS per 20 ms frame across the whole stream."""
    frame = sample_rate * FRAME_MS // 1000
    usable = len(samples) // frame * frame
    if usable == 0:
        return np.zeros(0, dtype=np.float32), frame
    frames = samples[:usable].reshape(-1, frame)
    return np.sqrt((frames**2).mean(axis=1)), frame


def speech_threshold(rms):
    """The level below which a region is 'basically empty'.

    This exists only to catch text invented over near-silence. It is NOT trying
    to separate speech from background, and an earlier version that tried to do
    that deleted eleven real sentences from a live test: audio bleeding from the
    speakers raised the measured floor above the speaker's own voice, and every
    quiet utterance was scored as noise.

    So the adaptive part may only ever LOWER the bar, never raise it. Keeping a
    stray hallucination is cheap — the enhancement step ignores it. Deleting
    something that was actually said is unrecoverable and invisible.
    """
    nonzero = rms[rms > 0]
    if len(nonzero) == 0:
        return ABSOLUTE_FLOOR
    quiet = float(np.percentile(nonzero, 10)) * 3.0
    return min(quiet, ABSOLUTE_FLOOR) if quiet > 0 else ABSOLUTE_FLOOR


def speech_ratio(rms, frame, threshold, start, end, sample_rate=16000):
    """Fraction of frames in [start, end) that are above the speech threshold."""
    lo = max(0, int(start * sample_rate) // frame)
    hi = min(len(rms), int(np.ceil(end * sample_rate / frame)))
    if hi <= lo:
        return 0.0
    return float((rms[lo:hi] > threshold).mean())


def transcribe_stream(path, offset, speaker, args, engine):
    samples = load_audio(path)
    duration = len(samples) / 16000
    if float(np.max(np.abs(samples))) < 1e-4:
        log(f"  {speaker:5s} {path.name}: silent ({duration:.1f}s) — skipping")
        return []

    rms, frame = frame_energy(samples)
    threshold = speech_threshold(rms)
    speech_frames = float((rms > threshold).mean()) if len(rms) else 0.0
    log(
        f"  {speaker:5s} {path.name}: {duration:.1f}s, "
        f"{speech_frames * 100:.0f}% above noise floor, transcribing…"
    )
    raw = engine.transcribe(
        path,
        language=None if args.language == "auto" else args.language,
        vocab=args.vocab,
    )

    # Everything below is engine-agnostic: the filtering works off the waveform
    # and the returned timestamps, not off any one engine's internals.
    segments, dropped = [], 0
    for segment in raw:
        text = segment.text.strip()
        if not text:
            continue
        if segment.no_speech_prob > 0.6 and segment.avg_logprob < -0.4:
            dropped += 1
            continue
        # Low energy alone is never grounds for dropping — it was, and it cost
        # real sentences. There has to be positive evidence of invention: either
        # the phrasing is a known Whisper filler, or the region is essentially
        # empty, or the model itself was unsure (checked above).
        ratio = speech_ratio(rms, frame, threshold, segment.start, segment.end)
        if HALLUCINATION_RE.match(text) and ratio < 0.35:
            dropped += 1
            continue
        if ratio < 0.02:
            dropped += 1
            continue
        segments.append(
            {
                "start": segment.start + offset,
                "end": segment.end + offset,
                "speaker": speaker,
                "text": text,
                "no_speech_prob": segment.no_speech_prob,
                "avg_logprob": segment.avg_logprob,
            }
        )

    log(f"  {speaker:5s} {len(segments)} segments kept, {dropped} dropped as silence/hallucination")
    return segments


def drop_echo(segments, threshold=0.8):
    """Remove ME segments that are just the far end bleeding into the mic.

    Only happens when the user is on speakers rather than headphones. Detected
    as a ME segment overlapping a THEM segment in time with near-identical text.
    """
    them = [s for s in segments if s["speaker"] == "THEM"]
    kept, removed = [], 0
    for segment in segments:
        if segment["speaker"] == "ME":
            echo = False
            for other in them:
                if segment["start"] < other["end"] and other["start"] < segment["end"]:
                    ratio = difflib.SequenceMatcher(
                        None, segment["text"].lower(), other["text"].lower()
                    ).ratio()
                    if ratio > threshold:
                        echo = True
                        break
            if echo:
                removed += 1
                continue
        kept.append(segment)
    if removed:
        log(f"  dropped {removed} ME segments as speaker echo of THEM")
    return kept


def coalesce(segments, max_gap=2.0):
    """Join consecutive same-speaker segments into turns."""
    turns = []
    for segment in segments:
        if turns and turns[-1]["speaker"] == segment["speaker"] and segment["start"] - turns[-1]["end"] <= max_gap:
            turns[-1]["text"] += " " + segment["text"]
            turns[-1]["end"] = segment["end"]
        else:
            turns.append(dict(segment))
    return turns


def timestamp(seconds):
    seconds = int(seconds)
    return f"{seconds // 3600:02d}:{seconds % 3600 // 60:02d}:{seconds % 60:02d}"


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("recording", type=Path, help="directory written by ./capture")
    parser.add_argument(
        "--engine",
        default=engines.DEFAULT,
        help=f"stt backend, e.g. whisper:large-v3, faster-whisper:large-v3, sarvam "
             f"(default: {engines.DEFAULT}; ./engines.py lists what is usable)",
    )
    parser.add_argument(
        "--language",
        default="auto",
        help=(
            "'auto' detects; 'hi' transcribes Hindi-dominant speech faithfully; "
            "'en' TRANSLATES non-English speech into English rather than transcribing it, "
            "which is silent and unrecoverable — see README (default: auto)"
        ),
    )
    parser.add_argument(
        "--vocab",
        default=None,
        help="comma-separated names/product terms to bias decoding toward",
    )
    parser.add_argument("--no-echo-filter", action="store_true")
    args = parser.parse_args()

    meta_path = args.recording / "capture.json"
    if not meta_path.exists():
        sys.exit(f"no capture.json in {args.recording}")
    meta = json.loads(meta_path.read_text())

    if args.vocab:
        # Whisper takes free text, not a word list; a sentence primes it better.
        args.vocab = "This meeting mentions: " + ", ".join(v.strip() for v in args.vocab.split(",")) + "."

    engine = engines.build(args.engine)
    log(f"engine {engine.describe()}")
    if engine.cloud:
        log("  WARNING: this engine uploads your meeting audio off this machine")
    segments = []
    for stream in meta["streams"]:
        path = args.recording / stream["path"]
        if not path.exists():
            log(f"  missing {path.name} — skipping")
            continue
        segments.extend(
            transcribe_stream(path, stream["offset_seconds"], stream["speaker"], args, engine)
        )

    segments.sort(key=lambda s: s["start"])
    if not args.no_echo_filter:
        segments = drop_echo(segments)
    turns = coalesce(segments)

    lines = [f"[{timestamp(t['start'])}] {t['speaker']}: {t['text']}" for t in turns]
    transcript = "\n".join(lines)

    (args.recording / "transcript.txt").write_text(transcript + "\n")
    (args.recording / "transcript.json").write_text(
        json.dumps({"engine": engine.describe(), "language": args.language, "turns": turns}, indent=2)
    )

    words = sum(len(t["text"].split()) for t in turns)
    log(f"\n  {len(turns)} turns, ~{words} words → {args.recording / 'transcript.txt'}")

    # Forcing English on non-English speech makes Whisper translate instead of
    # transcribe, and the result is fluent enough that nothing downstream can
    # tell. Say so rather than letting it pass silently.
    if args.language == "en":
        log("  note: --language en TRANSLATES non-English speech. If any of this "
            "meeting\n        was not in English, the words above are not what was said.")
    if any("ऀ" <= c <= "ॿ" for c in transcript):
        log("  note: transcript contains Devanagari — enhance.py will normalise it")
    if turns:
        log("\n  first few turns:")
        for line in lines[:5]:
            log("    " + line[:110])


if __name__ == "__main__":
    main()
