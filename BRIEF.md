# meetnote — spike brief

A fully-local, bot-free meeting notepad. Granola's core idea, running entirely on
this machine. **Generic personal tool — no company/employer specifics anywhere in it.**

This brief is a handover from a previous session. Everything below marked
*(verified)* was checked on this machine, not assumed.

## Goal

Capture a meeting's audio without a bot joining the call, transcribe it locally,
and use a local LLM to expand the user's own sparse typed notes into a complete
note — using the transcript as evidence, not writing a generic summary.

Eventually: store notes in SQLite and expose them over MCP so Claude Code can read
meeting notes directly. That's the real payoff, but it is **not** part of this spike.

## Non-goals for the spike

- No UI of any kind. Throwaway scripts.
- No MCP server yet.
- No cross-meeting search / RAG / embeddings yet.
- No calendar integration yet.
- No packaging, no signing, no distribution.

## Machine facts *(verified)*

| | |
|---|---|
| Chip | Apple M2 Max |
| RAM | 32 GB — already shows heavy pageouts under load |
| macOS | 26.3.1 (build 25D771280a) |
| Ollama | 0.18.2, server running on `localhost:11434` |
| Models pulled | `gpt-oss:20b` (13 GB), `nomic-embed-text` (274 MB), plus others |

The user runs a large Docker Compose stack for work on this machine, so RAM
headroom is not guaranteed. Design around that.

## Decisions already made (with rationale — don't re-litigate without reason)

**Audio capture: Core Audio process taps.** `AudioHardwareCreateProcessTap` /
`CATapDescription`, available macOS 14.4+. This machine is on 26.3.1 so it's
available. Chosen over ScreenCaptureKit because this is audio-only: the tap needs
no Screen Recording permission and shows no recording indicator. Chosen over
BlackHole/Loopback because it needs no virtual audio device installed.
Working sample code to lift: https://github.com/insidegui/AudioCap

**Capture mic and system audio as two separate streams.** Mic (`AVAudioEngine`) =
the user. Process tap = everyone else. Transcribe each separately, merge by
timestamp. This gives speaker separation for free — no pyannote, no diarization
model, no clustering. This is the single most important design decision in the
spike; don't collapse the two streams into one file.

**STT: Whisper large-v3-turbo, not Parakeet TDT.** Parakeet is 2–5× faster on
Apple Silicon and matches Whisper on clean English, but covers English + European
languages only. The user's meetings are Indian-English with Hinglish
code-switching. Whisper's messier training data is what makes it robust to
accented and code-switched audio. Speed is not the constraint here — large-v3-turbo
does an hour of audio in well under a minute on an M2 Max.
Implementation: `mlx-whisper` (Python, simplest) or `whisper.cpp` (easier to embed later).

**LLM: `gpt-oss:20b` via Ollama.** OpenAI's open-weight model, Apache 2.0.
Already pulled. The user has an explicit constraint: **no Chinese-origin models** —
so avoid qwen / deepseek / glm / kimi even though several are already pulled on
this machine. Non-Chinese fallbacks if needed: `mistral-small` (Mistral, France,
Apache 2.0), `gemma3` (Google).

**Enhancement runs once, after the meeting ends — not live.** 13 GB of weights plus
KV cache on a 32 GB machine that may be running Docker means live enhancement
would fight for RAM during the call. Transcribe during, enhance after.

## Known gotchas

- **`num_ctx` will silently truncate.** A one-hour meeting is ~12–18k tokens.
  Ollama's default context window is far smaller and will drop the tail of the
  transcript **without any error** — you get a confident summary of the first
  fifteen minutes. Set `num_ctx: 32768` explicitly in the request options.
- If the enhancement pass is sluggish, stopping Docker first is the fix.
- Core Audio taps need `NSAudioCaptureUsageDescription` in the Info.plist and will
  trigger a TCC consent prompt on first run.

## The enhancement prompt is the product

The "your notes steer the output" behaviour is not a model capability — it's
instruction design. Pass the transcript *and* the user's raw fragments, then
instruct roughly:

> Below are the user's own notes, typed during the meeting. They are terse and are
> the ONLY signal of what mattered. Expand each fragment into a complete point
> using the transcript as source material. Preserve their ordering and emphasis.
> Do not add sections they did not gesture at. If a fragment is unsupported by the
> transcript, keep it verbatim and mark it unverified.

Getting this prompt right matters more than any other code in the project.

## Spike scope

Three throwaway pieces:

```
capture.swift    → two WAVs: mic stream + system-audio tap stream
transcribe.py    → mlx-whisper on each, merge by timestamp into a labelled transcript
enhance.py       → transcript + hand-written scratch notes → ollama gpt-oss:20b → note
```

Then run it on one real, messy, accented call and judge the output.

## Success criteria — what the spike must actually answer

1. Does the Core Audio tap reliably capture the far end of a Zoom/Meet/Teams call?
2. Is the transcript usable on accented, code-switched, real-world audio — or does
   Whisper mangle names, product terms, and Hinglish enough to poison the summary?
3. Does the two-stream merge actually produce clean me/them attribution?
4. Does `gpt-oss:20b` genuinely expand sparse notes, or does it drift into writing
   a generic meeting summary regardless of prompt?

Question 2 is the one most likely to kill the project. Test it early on real audio,
not on a clean read-aloud sample.

## If the spike succeeds

Productisation order: SQLite storage → MCP server (~200 lines, the actual payoff) →
EventKit calendar detection → templates → menu-bar app. Not before.
