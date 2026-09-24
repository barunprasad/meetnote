# meetnote — architecture

Design for the real application. `BRIEF.md` holds the original decisions,
`RESULTS.md` what the spike actually proved. This is what gets built.

Where a decision below looks arbitrary, it is usually load-bearing because
something failed during the spike. Those are marked. Nothing here re-opens a
question the spike already answered.

## Stack

macOS 14.4+, Apple Silicon, fully local.

| layer | tech | why |
|---|---|---|
| UI | Tauri v2 + React/TS | fastest to build in, for this author |
| Orchestration, storage, LLM | Rust (`src-tauri`) | Tauri's backend; owns SQLite and Ollama |
| Audio capture | **Swift sidecar** | Core Audio process taps exist nowhere else |
| Transcription | **`whisper-rs`, in-process** | whisper.cpp is a C library — Rust calls it directly, with Metal/CoreML |
| MCP server | Rust binary | shares the storage code; runs standalone |

Three languages is the real cost of this stack, and it is a deliberate trade:
the capture and transcription layers are Swift-only regardless of what wraps
them, so Rust sits in the middle owning orchestration rather than algorithms.
It is accepted because UI velocity dominates on a personal tool.

```
MeetNote.app
├─ Contents/MacOS/
│   ├─ meetnote                   Tauri binary (Rust + WKWebView)
│   ├─ meetnote-capture           Swift sidecar — Core Audio tap + mic
│   └─ meetnote-mcp               Rust binary — stdio MCP, opens DB read-only
│                                 (transcription and diarisation are in-process)
└─ Contents/Info.plist            usage strings live HERE, see below
```

## The permission spike — RESOLVED, the design holds

**This was the highest risk in the design, and it is not the UI.** It has been
built and run (`tcc-spike/`). A Swift sidecar spawned by a signed Tauri app
**does** inherit the parent bundle's TCC grants:

```
mic    : peak -28.5 dBFS
system : peak  -3.0 dBFS      (audio the app played itself, captured via the tap)
469 IOProc callbacks, 0 dropped buffers, 6 ms inter-stream offset
```

Both prompts appeared, attributed to the Tauri app, using the usage strings from
`src-tauri/Info.plist` — which Tauri merges into the bundle's generated plist
without extra configuration. The three-language architecture stands.

Three things the spike settled that the build should keep:

- **Usage strings go on the parent app**, not the sidecar. Confirmed working.
- **Do not `codesign --deep`.** Tauri bundles the sidecar to
  `Contents/MacOS/meetnote-capture` (target triple stripped) *without*
  re-signing it, so it keeps its own identifier. A `--deep` re-sign would
  overwrite that.
- **Set a stable signing identifier.** Tauri's default output is linker-signed
  as `tcc_spike-9d2efc4ad1233fc6` — derived from the binary hash, so it changes
  on every rebuild and TCC grants evaporate with it. Sign the outer bundle
  explicitly (`--identifier local.meetnote.app`, no `--deep`).

A fourth lesson is about testing, not building: **the spike must play its own
audio while capturing.** The first run reported the system stream silent, which
looked like a denial and was not — nothing had been playing, and a working tap
over silence is byte-identical to a refused one. Any future capture test should
generate its own known sound.

### Original risk assessment (kept for context)

macOS attributes a child process's TCC requests to the *responsible process*.
During the spike, a bare CLI binary could not obtain an audio-capture grant at
all — Warp never surfaced the prompt, `AVCaptureDevice.requestAccess` returned
false immediately, and the capture came back as digital silence with no error
anywhere. It only worked once the binary was inside a signed `.app` launched by
`open`.

That fix now has to hold across an extra hop: **Tauri app → spawned Swift
sidecar**. The expectation is that the sidecar inherits the parent bundle's TCC
identity, so `NSAudioCaptureUsageDescription` and `NSMicrophoneUsageDescription`
must be in the *Tauri app's* `Info.plist`, not the sidecar's. That is the theory.
It is unverified, and this project has already lost hours to TCC twice.

So: before any UI work, build the smallest possible Tauri app that spawns the
existing `capture.swift` binary as a sidecar and records five seconds. If
`peak_dbfs` comes back as real audio rather than silence, the architecture is
sound. If not, capture has to move into the Rust process via FFI, or the app
becomes Swift after all — and it is much cheaper to learn that in an afternoon
than after the UI is built.

## Sidecar contracts

Sidecars are plain CLI programs speaking newline-delimited JSON on stdout, which
is what Tauri's sidecar API consumes. No FFI, no shared memory.

### `meetnote-capture`

```
meetnote-capture --out <dir> --meeting-id <uuid> [--app <substring>]
```

Streams status while running, stops on a `stop` line on stdin or SIGINT:

```json
{"type":"ready","streams":{"mic":{"rate":48000,"ch":1},"system":{"rate":48000,"ch":2}}}
{"type":"level","t":3.0,"mic_dbfs":-38.2,"system_dbfs":-12.1}
{"type":"done","streams":[
  {"label":"mic","path":"mic.wav","rate":48000,"ch":1,
   "offset_secs":0.0,"duration_secs":1830.4,"peak_dbfs":-24.6},
  {"label":"system","path":"system.wav","rate":48000,"ch":2,
   "offset_secs":0.003,"duration_secs":1830.4,"peak_dbfs":-11.8}]}
```

`level` events drive the UI meter. They are not cosmetic: the spike's single
most confusing failure was a capture that ran perfectly and produced silence, and
a live meter makes that visible within seconds instead of after the meeting.

Port from `capture.swift` unchanged, including three fixes that must survive:

- **Never set `kAudioAggregateDeviceTapAutoStartKey`.** With it, the IOProc is
  never invoked when output is Bluetooth. Measured: 0 callbacks with it, ~188 in
  2 s without, everything else constant. No error is reported either way.
- **Start the mic before building the tap's aggregate device.** Opening an input
  on a Bluetooth headset makes macOS recreate the output device; building the
  aggregate first binds it to a device that is about to vanish.
- **Record each stream's first-buffer host time.** The entire merge rests on it.

### Transcription and diarisation — in-process, not sidecars

Originally specified as a Swift sidecar wrapping WhisperKit. Replaced: whisper.cpp
is a C library, so `whisper-rs` calls it directly with Metal/CoreML, and
`speakrs` does diarisation in Rust. That removed a language and an IPC boundary,
and quantised GGML cut the model download from ~1.6 GB to ~547 MB — something
WhisperKit's CoreML packaging could not have offered.

Three behaviours must survive any future change of engine, all learned the hard
way:

- **Never default to `--language en`.** Forcing English on Hindi-dominant speech
  makes Whisper *translate* rather than transcribe. The output is fluent,
  confident, and nothing downstream can tell the real words are gone. Auto-detect.
- **Transcribe faithfully; normalise in the LLM.** Devanagari in the transcript
  is correct. The enhancement step renders the note in the user's language with
  the whole transcript as context, which beats a decoder working 30 s at a time.
- **Energy filtering only catches invention over near-silence.** It must never
  try to separate speech from background. Audio bleeding from speakers raises the
  measured noise floor above the speaker's own voice; an earlier version did
  exactly that and deleted 11 of 14 real sentences. Low energy alone is not
  grounds for dropping anything — there must be positive evidence of invention,
  and `whisper-rs` exposes the model's own `no_speech_probability` for it.

Diarisation runs on the **system stream only**: which file the audio arrived on
already answers whether a voice is the user, so clustering only has to split
`THEM`. `ME` is never relabelled.

## Data model

SQLite via `rusqlite` at `~/Library/Application Support/MeetNote/meetnote.db`,
audio at `recordings/<uuid>/`. WAL mode, so the MCP reader never blocks a write.

```sql
CREATE TABLE meeting (
    id          TEXT PRIMARY KEY,
    started_at  INTEGER NOT NULL,
    ended_at    INTEGER,
    title       TEXT,
    source      TEXT,                        -- zoom|meet|teams|phone|unknown
    audio_dir   TEXT NOT NULL,
    status      TEXT NOT NULL,               -- recording|transcribing|enhancing|done|failed
    error       TEXT
);

CREATE TABLE stream (
    meeting_id   TEXT NOT NULL REFERENCES meeting(id) ON DELETE CASCADE,
    label        TEXT NOT NULL,              -- mic|system
    path         TEXT NOT NULL,
    sample_rate  INTEGER NOT NULL,
    channels     INTEGER NOT NULL,
    offset_secs  REAL NOT NULL,              -- host-clock offset; the merge needs it
    duration_secs REAL,
    peak_dbfs    REAL,                       -- a denied tap is silent, not an error
    PRIMARY KEY (meeting_id, label)
);

CREATE TABLE turn (
    meeting_id   TEXT NOT NULL REFERENCES meeting(id) ON DELETE CASCADE,
    idx          INTEGER NOT NULL,
    start_secs   REAL NOT NULL,
    end_secs     REAL NOT NULL,
    speaker      TEXT NOT NULL,              -- ME|THEM-1|THEM-2…
    speaker_name TEXT,                       -- resolved later, from calendar
    text         TEXT NOT NULL,
    PRIMARY KEY (meeting_id, idx)
);

-- The user's live fragments, timestamped. The spike's notes.md was not.
CREATE TABLE note_fragment (
    meeting_id  TEXT NOT NULL REFERENCES meeting(id) ON DELETE CASCADE,
    idx         INTEGER NOT NULL,
    at_secs     REAL NOT NULL,               -- when it was typed, into the meeting
    text        TEXT NOT NULL,
    PRIMARY KEY (meeting_id, idx)
);

CREATE TABLE enhanced_note (
    meeting_id     TEXT PRIMARY KEY REFERENCES meeting(id) ON DELETE CASCADE,
    body           TEXT NOT NULL,
    model          TEXT NOT NULL,
    prompt_version INTEGER NOT NULL,
    created_at     INTEGER NOT NULL
);

CREATE VIRTUAL TABLE note_fts USING fts5(title, body, content='');

-- Credentialed providers only. Ollama is detected, not configured, so it never
-- has a row here. The key itself lives in the Keychain, never in this file.
CREATE TABLE llm_provider (
    name        TEXT PRIMARY KEY,            -- anthropic | openai | …
    models      TEXT NOT NULL,               -- json array of selected model ids
    label       TEXT,
    verified_at INTEGER                      -- last successful verify()
);
```

Three deliberate choices:

**`note_fragment.at_secs` is new and is the cheapest quality win available.** The
spike passed notes as an undifferentiated list, so the model had to search the
whole transcript for what each fragment referred to. Knowing a fragment was typed
at 14:32 anchors it to the transcript around that moment. The enhancement prompt
is the product; this makes it materially better for almost no work.

**`prompt_version`**, because the prompt is the product and will change. Notes
written under an old prompt must stay identifiable.

**`stream.peak_dbfs`**, because a denied tap yields a full-length file of digital
silence with no error anywhere. That has to be visible in the data.

## Orchestration (Rust)

State machine per meeting, persisted at every transition so a crash is
recoverable:

```
recording ──> transcribing ──> enhancing ──> done
     └──────────────┴───────────────┴─────> failed (error recorded)
```

- The `meeting` row is written at record start, so a crash leaves a recoverable
  artefact rather than an orphaned directory of wav files.
- On launch, any meeting still marked `recording` is re-queued for transcription
  from whatever audio exists on disk.
- **Enhancement runs after the meeting, never during.** `gpt-oss:20b` is 13 GB;
  it must not compete with the call. Transcription may run during, since
  WhisperKit on the ANE is cheap, but the default is after.
- `num_ctx: 32768` on every Ollama call, explicitly. The default truncates the
  transcript with no error and yields a confident note about the first fifteen
  minutes.

## UI (React/TS)

Two surfaces, and only two for v1:

1. **Tray item** — start/stop, current state, live level meters from the capture
   sidecar's `level` events.
2. **Floating notes panel** — always-on-top, one text field. Every line typed
   becomes a `note_fragment` with its timestamp. This panel *is* the product
   surface: sparse fragments typed live are the entire input signal that makes
   the output a note rather than a summary.

History, search and editing are deliberately out of v1. They may belong in Claude
Code via MCP rather than in the app at all, and building them before knowing that
is how this turns into a worse Granola instead of a better notepad.

## MCP server

Standalone Rust binary, stdio, read-only connection to the same DB. Ships inside
the bundle but runs independently, so it works whether or not the app is open and
cannot corrupt a recording in progress.

| tool | purpose |
|---|---|
| `list_meetings(since, limit)` | recent meetings: title, date, status |
| `get_note(meeting_id)` | the enhanced note |
| `search_notes(query)` | FTS5 across notes |
| `get_transcript(meeting_id, from, to)` | raw turns, windowed |

`get_transcript` is windowed deliberately: an hour of dialogue is 12–18k tokens
and returning it whole will blow out a context window.

## Build and distribution

Tauri bundles sidecars as `externalBin` with target-triple suffixes
(`meetnote-capture-aarch64-apple-darwin`). They are signed as part of the app
bundle. A stable signing identifier matters — the spike confirmed TCC grants
survive rebuilds only when the identifier is stable, otherwise every build
re-prompts.

Ad-hoc signing is fine for personal use. Developer ID and notarisation only
matter for distribution, which is not a goal.

## Build order

1. **TCC spike** — Tauri app spawning the existing capture binary as a sidecar.
   Nothing else until real audio comes back.
2. SQLite schema + Rust storage layer.
3. ~~`meetnote-stt` sidecar (WhisperKit)~~ — done differently: in-process `whisper-rs`.
4. Enhancement: the `LlmProvider` trait with the Ollama impl only, the prompt
   ported verbatim, and `note_fragment` anchoring. Cloud providers come later —
   the seam is what matters now, not the second implementation.
5. MCP server — the actual payoff, per the brief.
6. UI: tray, then notes panel.
7. SpeakerKit diarisation, EventKit meeting detection.

## State — everything below is built and installed

`~/Applications/meetnote.app`. Record → transcribe → diarise → note, with no
manual step in between.

| crate | what it does | tests |
|---|---|---|
| `core` | SQLite storage, shared with the future MCP server | 8 |
| `stt` | whisper.cpp in-process, two streams merged on one timeline | 5 |
| `llm` | the enhancement prompt + Ollama behind a provider trait | 5 |
| `diarize` | speakrs, splitting `THEM` into individual speakers | 5 + benchmark |
| `app` | Tauri: capture sidecar, orchestration, UI | — |
| `mcp` | stdio MCP server, read-only — **the payoff** | 2 |

**Diarisation is tuned, not guessed.** `cargo test -p meetnote-diarize --release
-- --ignored` runs an automated benchmark against `fixtures/demo`, which carries
two known far-end speakers across eight turns. It found the default clustering
threshold splits two people into four:

```text
ahc_threshold   speakers   accuracy
  0.60 default     4 / 2      93.5%
  0.80 chosen      2 / 2      99.1%
  0.90             1 / 2      38.4%
```

Two things that matters for: the usable window is one step wide, and the fixture
is TTS — far more consistent than real accented speech over a compressed call.
**Re-run the sweep on real audio before trusting 0.8.** It takes five seconds.

Diarisation is best-effort by design: if it fails, the meeting still gets a
transcript with an undifferentiated `THEM`, because a labelled transcript is
worth much less than a transcript.

### Reading notes from Claude Code

`meetnote-mcp` ships inside the app bundle and runs as its own process:

```sh
claude mcp add meetnote ~/Applications/meetnote.app/Contents/MacOS/meetnote-mcp
```

Four tools — `list_meetings`, `get_note`, `search_notes`, `get_transcript`.
Two decisions in there are worth keeping:

- **The database is opened read-only**, enforced by the connection rather than
  by care, so a question asked mid-meeting cannot touch a recording in progress.
  It also works whether or not the app is running: the notes are just a file.
- **`get_transcript` is windowed by minute and capped at 400 lines.** An hour of
  dialogue is 12–18k tokens; returning it whole would blow out the caller's
  context, which is the failure that makes an MCP server useless in practice.

The server's `instructions` tell the model how far to trust each layer — that
`ME` is a fact about which microphone the audio came from, while `THEM-n` is a
model's guess, and that an unverified line is the user's words rather than an
established fact. `tests/protocol.rs` asserts those instructions survive, drives
a real handshake over stdio, and fails if anything but protocol reaches stdout.

### Known dependency pin

`tauri-runtime` is pinned to `=2.4.0`. `tauri-runtime-wry 2.4.0` is the newest
published and does not implement the trait methods `tauri-runtime 2.5+` added,
so the pair will not compile unpinned. Remove the pin once wry catches up.

## Open questions

1. **Retention.** Audio is ~350 MB/hour at 48 kHz stereo. Deleting after
   transcription saves disk; keeping it makes future engine upgrades retroactive.
2. **The Python harness.** Keep `transcribe.py` / `engines.py` /
   `compare_engines.py` as an unshipped experimentation bench for Sarvam and
   faster-whisper comparisons? Recommendation: yes. Engine comparison stays
   useful and does not belong in the app.
3. **Echo suppression.** The text-similarity filter misses partial overlaps —
   verified: a video's words landed in the ME stream while on speakers. Proper
   cross-correlation between the two streams would make the tool usable without
   headphones.
