#!/usr/bin/env python3
"""enhance.py — expand sparse live notes into a full note, using the transcript.

    ./enhance.py recordings/2026-08-14-234500 --notes notes.md

The point is NOT to summarise the meeting. The user's own fragments are the
outline and the only signal of what mattered; the transcript is source material
for filling them in. Everything that makes that happen is in PROMPT below.

Stdlib only — runs on system python, no venv needed.
"""

import argparse
import json
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

OLLAMA = "http://localhost:11434"
MODEL = "gpt-oss:20b"

# A one-hour meeting is ~12-18k tokens. Ollama's default context is far smaller
# and truncates the transcript with NO error — you get a confident note about
# the first fifteen minutes. This must be set explicitly.
NUM_CTX = 32768

SYSTEM = """You expand a person's own meeting notes into a complete note.

You are given a transcript of a meeting and the terse fragments that person \
typed while it was happening. Their fragments are an outline of what they \
considered worth recording. Your job is to expand each fragment into something \
complete and useful, drawing the substance from the transcript.

You are not writing a summary of the meeting. A summary covers what the meeting \
was about; you are covering what THEY thought was worth writing down. These are \
different documents, and producing the first one when asked for the second is \
the main way this task is failed."""

PROMPT = """## Transcript

Speaker labels: ME is the person whose notes these are. THEM is everyone else \
on the call.

{transcript}

## Their notes, typed live during the meeting

{notes}

## Instructions

1. Their fragments are the outline. Work through them in the order they were \
written, one section per fragment. Keep their ordering and their emphasis — a \
fragment they wrote three words about may deserve a short section, and one they \
wrote nothing about does not get a section at all.
2. Expand each fragment using the transcript as source material. Pull in the \
specifics that fragment was pointing at: the numbers, names, dates, decisions, \
objections and commitments that appear in the transcript around that topic.
3. Write the note in your own words, as settled statements of fact. Do not quote \
the transcript, do not copy sentences of speech across, and strip conversational \
filler and discourse markers. The reader wants the substance of what was \
established, not a trimmed-down replay of the dialogue. Never state the same \
fact twice.
4. Keep their vocabulary — if they used a shorthand, an abbreviation or an \
internal term, use it rather than translating it into neutral language. This is \
about their terminology, not a licence to copy speech out of the transcript.
5. The transcript is code-switched and may be partly in Devanagari, because it \
was transcribed faithfully rather than translated. Write the note in the \
language their own notes are written in — normally English. Translate or \
transliterate as needed, but keep names, product terms and any phrase whose \
exact wording carries weight. Never drop a point because it was said in another \
language, and never leave a sentence untranslated in the finished note.
6. Attribute decisions and commitments to whoever made them. Refer to the person \
whose notes these are in the first person; name other participants where the \
transcript names them, and otherwise describe them by role.
7. Do not add sections for topics they did not gesture at, no matter how much \
time the meeting spent on them. Silence in their notes is a signal, not a gap \
to fill.
8. If a fragment is not supported anywhere in the transcript, still give it its \
section, and write exactly `_(unverified — not discussed in the transcript)_` as \
the body. Do not quietly drop it and do not invent support for it.
9. Never invent a name, number, date, or commitment that is not in the \
transcript.
{extra}
Output markdown. Give each section a `##` heading — the fragment itself, as they \
wrote it — followed by the expanded prose on its own line beneath. Start directly \
with the first heading: no preamble, no restatement of these instructions, no \
closing commentary."""

ACTION_ITEMS = """10. After the sections, add an "Action items" list of \
commitments actually stated in the transcript, noting who owns each. Rule 7 \
still applies here: only commitments belonging to the topics they noted down. A \
commitment made about a topic they did not gesture at does not earn a line in \
this list — that is the generic-summary failure returning through a side door. \
If nothing qualifies, leave the list out entirely.
"""


def estimate_tokens(text):
    return len(text) // 4


def check_ollama(model):
    try:
        with urllib.request.urlopen(f"{OLLAMA}/api/tags", timeout=5) as response:
            tags = json.load(response)
    except urllib.error.URLError as error:
        sys.exit(f"cannot reach ollama at {OLLAMA} ({error}). Is `ollama serve` running?")
    names = [m["name"] for m in tags.get("models", [])]
    if model not in names and f"{model}:latest" not in names:
        sys.exit(f"model {model} not pulled. Available: {', '.join(names)}\n  try: ollama pull {model}")


def generate(model, system, prompt, temperature):
    body = json.dumps(
        {
            "model": model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": prompt},
            ],
            "stream": True,
            "options": {
                "num_ctx": NUM_CTX,
                "temperature": temperature,
                "top_p": 0.9,
            },
        }
    ).encode()
    request = urllib.request.Request(
        f"{OLLAMA}/api/chat", data=body, headers={"Content-Type": "application/json"}
    )

    chunks = []
    thinking = False
    try:
        with urllib.request.urlopen(request, timeout=1800) as response:
            for line in response:
                if not line.strip():
                    continue
                event = json.loads(line)
                message = event.get("message", {})
                # gpt-oss is a reasoning model; Ollama surfaces its scratchpad
                # separately. Show progress for it but keep it out of the note.
                if message.get("thinking"):
                    if not thinking:
                        print("  thinking", end="", file=sys.stderr, flush=True)
                        thinking = True
                    print(".", end="", file=sys.stderr, flush=True)
                token = message.get("content", "")
                if token:
                    if thinking:
                        print("", file=sys.stderr, flush=True)
                        thinking = False
                    chunks.append(token)
                    print(token, end="", flush=True)
                if event.get("done"):
                    break
    except urllib.error.URLError as error:
        sys.exit(f"\nollama request failed: {error}")
    return "".join(chunks)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("recording", type=Path, help="directory containing transcript.txt")
    parser.add_argument("--notes", type=Path, help="your live notes (default: <recording>/notes.md)")
    parser.add_argument("--model", default=MODEL)
    parser.add_argument("--temperature", type=float, default=0.3)
    parser.add_argument("--action-items", action="store_true", help="append an action-item list")
    parser.add_argument("--out", type=Path, help="default: <recording>/note.md")
    args = parser.parse_args()

    transcript_path = args.recording / "transcript.txt"
    if not transcript_path.exists():
        sys.exit(f"no transcript.txt in {args.recording} — run ./transcribe.py first")
    transcript = transcript_path.read_text().strip()

    notes_path = args.notes or args.recording / "notes.md"
    if not notes_path.exists():
        sys.exit(
            f"no notes at {notes_path}.\n"
            "  This tool expands YOUR notes — without them there is nothing to steer the\n"
            "  output and you would just get a generic summary, which is the thing we are\n"
            "  explicitly not building."
        )
    notes = notes_path.read_text().strip()
    if not notes:
        sys.exit(f"{notes_path} is empty — see above.")

    check_ollama(args.model)

    prompt = PROMPT.format(
        transcript=transcript,
        notes=notes,
        extra=ACTION_ITEMS if args.action_items else "",
    )
    tokens = estimate_tokens(SYSTEM + prompt)
    fragments = len([line for line in notes.splitlines() if line.strip()])
    print(
        f"  {args.model}, num_ctx {NUM_CTX}\n"
        f"  transcript {len(transcript.splitlines())} turns, {fragments} note fragments, "
        f"~{tokens} tokens in\n",
        file=sys.stderr,
    )
    if tokens > NUM_CTX * 0.9:
        print(
            f"  WARNING: input is close to num_ctx ({tokens} vs {NUM_CTX}); the tail of the\n"
            f"  transcript may be silently dropped. Raise NUM_CTX or trim the transcript.\n",
            file=sys.stderr,
        )

    started = time.time()
    note = generate(args.model, SYSTEM, prompt, args.temperature)
    elapsed = time.time() - started

    out_path = args.out or args.recording / "note.md"
    out_path.write_text(note.strip() + "\n")
    print(f"\n\n  {elapsed:.0f}s → {out_path}", file=sys.stderr)
    if elapsed > 300:
        print("  (slow — stopping Docker frees the RAM this needs)", file=sys.stderr)


if __name__ == "__main__":
    main()
