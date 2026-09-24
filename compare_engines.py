#!/usr/bin/env python3
"""compare_engines.py — run one recording through several STT engines and diff.

    ./compare_engines.py recordings/<ts> whisper:large-v3-turbo whisper:large-v3-8bit

Writes transcript.<engine>.txt per engine and prints them turn by turn so you
can see exactly which words each one got wrong. This is the point of the
pluggable engine layer: answering "does anything handle Hinglish" should be one
command, not a rewrite.

Each engine runs in its own subprocess, deliberately. Model weights are freed
when the process exits, so comparing four engines never holds four models in
memory at once — which matters on a machine that is already swapping.

Stdlib only.
"""

import argparse
import re
import subprocess
import sys
from pathlib import Path

VENV_PYTHON = Path(__file__).parent / ".venv" / "bin" / "python"
TURN_RE = re.compile(r"^\[(\d\d:\d\d:\d\d)\]\s+(ME|THEM):\s*(.*)$")


def run_engine(recording, engine, extra):
    """Transcribe with one engine, in a fresh process, and stash the result."""
    python = str(VENV_PYTHON) if VENV_PYTHON.exists() else sys.executable
    command = [python, str(Path(__file__).parent / "transcribe.py"), str(recording),
               "--engine", engine] + extra
    print(f"  running {engine} …", file=sys.stderr, flush=True)
    result = subprocess.run(command, capture_output=True, text=True)
    if result.returncode != 0:
        print(f"  {engine} FAILED:\n{result.stderr[-800:]}", file=sys.stderr)
        return None
    produced = recording / "transcript.txt"
    if not produced.exists():
        return None
    saved = recording / f"transcript.{engine.replace(':', '-').replace('/', '-')}.txt"
    saved.write_text(produced.read_text())
    return saved


def parse(path):
    turns = []
    for line in path.read_text().splitlines():
        match = TURN_RE.match(line)
        if match:
            turns.append((match.group(1), match.group(2), match.group(3)))
    return turns


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("recording", type=Path)
    parser.add_argument("engines", nargs="+", help="engine specs, e.g. whisper:large-v3-8bit")
    parser.add_argument("--language", default="en")
    parser.add_argument("--vocab", default=None)
    parser.add_argument("--width", type=int, default=96)
    args = parser.parse_args()

    extra = ["--language", args.language]
    if args.vocab:
        extra += ["--vocab", args.vocab]

    results = {}
    for engine in args.engines:
        saved = run_engine(args.recording, engine, extra)
        if saved:
            results[engine] = parse(saved)

    if not results:
        sys.exit("no engine produced a transcript")

    print()
    for engine, turns in results.items():
        words = sum(len(t[2].split()) for t in turns)
        print(f"  {engine:<28} {len(turns):>3} turns, {words:>5} words")
    print()

    # Turn-by-turn, so differences in wording are obvious at a glance.
    longest = max(len(t) for t in results.values())
    for index in range(longest):
        stamps = {e: (t[index] if index < len(t) else None) for e, t in results.items()}
        reference = next((v for v in stamps.values() if v), None)
        print(f"── turn {index + 1}  [{reference[0]}] {reference[1]}")
        for engine, turn in stamps.items():
            text = turn[2] if turn else "(no turn)"
            print(f"   {engine:<26} {text[:args.width]}")
            for start in range(args.width, len(text), args.width):
                print(f"   {'':<26} {text[start:start + args.width]}")
        print()


if __name__ == "__main__":
    main()
