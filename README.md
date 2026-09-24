# meetnote — spike

Fully-local, bot-free meeting notepad. See `BRIEF.md` for the design decisions
and the reasoning behind them; this file is just how to run it.

## Setup

```sh
./build.sh                                  # builds ./capture (needs Xcode)
python3.13 -m venv .venv                    # mlx has no 3.14 wheels yet
./.venv/bin/pip install mlx-whisper
```

`enhance.py` is stdlib-only and needs no venv — just Ollama running.

## Run

```sh
./record.sh 30                              # launches the .app so macOS prompts
./capture                                   # direct CLI run, once permissions exist
                                            # → recordings/<ts>/{mic,system}.wav + capture.json

./.venv/bin/python transcribe.py recordings/<ts>
                                            # → transcript.txt, transcript.json

$EDITOR recordings/<ts>/notes.md            # your sparse live notes go here
python3 enhance.py recordings/<ts>          # → note.md
```

Useful flags:

| flag | why |
|---|---|
| `capture --list` | which processes Core Audio can see, and which are playing |
| `capture --pid N` | tap one app instead of everything (fallback if a global tap misbehaves) |
| `capture --duration N` | stop automatically, for scripted tests |
| `transcribe.py --engine faster-whisper:large-v3` | swap the STT engine (see below) |
| `transcribe.py --vocab "Priya,Kafka,idempotency"` | bias decoding toward names and jargon it keeps mangling |
| `transcribe.py --language auto` | let Whisper detect per-window; see the Hinglish note below |
| `enhance.py --action-items` | append an action-item list (off by default, and see below) |

`--action-items` needs watching. In testing, the first version of that instruction
quietly reintroduced a topic the user had never noted — it listed a commitment
someone made during a stretch of the meeting the user ignored. An action-item
list is a summary of the whole meeting unless you explicitly scope it to the
topics the fragments gesture at, which is the same failure the whole design is
trying to avoid, arriving through a side door. The instruction now says so
outright.

## First run: audio permission

**Use `./record.sh`, not `./capture`, until permissions are granted.**

A bare CLI binary cannot own a TCC permission. macOS attributes its requests to
the *responsible process* — the terminal that launched it — and if that terminal
will not surface the prompt, TCC auto-denies without displaying anything and
without returning an error. Warp does exactly this: `AVCaptureDevice` reports
`notDetermined`, `requestAccess` returns false immediately, no prompt is ever
drawn, and the recording is full of zeros.

`build.sh` therefore produces a signed `meetnote-capture.app`, and `record.sh`
launches it with `open` so launchd owns it. The app is then responsible for
itself, gets its own TCC identity, and the prompt appears with the usage strings
from `Info.plist`. This is why AudioCap is an app rather than a CLI tool.

Microphone and Audio Recording are two separate grants and prompt separately.
Approve both.

The first `./capture` triggers a macOS consent prompt for recording this
computer's audio. **It has to be run from a real terminal window you can click
in** — if the prompt is never answered, macOS does not return an error. It hands
back a working tap that produces pure digital silence.

So if `mic.wav` / `system.wav` come out silent, check
System Settings → Privacy & Security → **Audio Recording** and **Microphone**,
confirm your terminal is listed and enabled, and re-run. `capture` prints a
warning when it detects an empty system stream.

`./build.sh` must be used rather than a plain `swiftc` — the usage strings have
to be linked into the binary's `__TEXT,__info_plist` section or the prompt never
appears at all.

## Testing without a meeting

```sh
python3 make_fixture.py fixtures/demo       # scripted TTS dialogue, two streams
```

This exercises the merge, the silence filtering and the enhancement prompt
end-to-end. It does **not** tell you whether Whisper survives real accented,
code-switched audio — TTS is far too clean. Only a real call answers that, and
per the brief it is the question most likely to kill the project.

If a fixture ever transcribes as nonsense, regenerate it before suspecting
Whisper. macOS ships voice *metadata* before voice *data*, so the first `say`
with an `en_IN` voice renders a differently-timed fallback, exits 0, and fetches
the real voice in the background — producing audio that silently disagrees with
the layout the generator thinks it wrote. The generator now warms each voice
first, but the same trap applies to any new voice you add.

## Phone calls

Mostly already works — there is no separate feature to build. A global tap
captures whatever this Mac plays, and with Continuity enabled an iPhone call is
relayed through the Mac's Core Audio like any other app. Both relay daemons are
visible as tappable processes:

```
com.apple.TelephonyUtilities    iPhone cellular calls relayed to this Mac
com.apple.avconferenced         FaceTime
```

Your side still comes from the Mac's microphone, so the two-stream ME/THEM split
is unchanged. To exclude everything else playing on the machine:

```sh
./capture --app TelephonyUtilities      # or --app avconferenced for FaceTime
```

Requirements and limits:

- **Continuity must be on** — iPhone → Settings → Phone → Calls on Other
  Devices, enabled for this Mac. Otherwise the call never reaches Core Audio.
- **A call answered on the iPhone itself is not capturable.** iOS does not
  permit it and the Mac cannot reach it. There is no workaround.
- **Narrowband audio.** Cellular voice is 8-16 kHz through AMR or EVS, much
  harsher than a Zoom call's Opus. This cuts both ways as a test: surviving it
  implies surviving Zoom, but failing it does not condemn Zoom.
- **Consent.** Recording a call has two-party consent requirements in many
  jurisdictions, India included. Tell the other person.

**Unverified:** the daemons are tappable and `--app` targets them, but whether
call audio actually produces non-zero samples through that path needs a real
call to confirm.

## Swapping the STT engine

Whisper is the default, not a commitment. The brief names accented,
code-switched audio as the most likely thing to kill this project, which makes
the STT engine the component most likely to be replaced — so it is the one part
that is not wired in by hand.

```sh
./engines.py                                          # what is usable right now
./.venv/bin/python transcribe.py <rec> --engine whisper:large-v3
./.venv/bin/python transcribe.py <rec> --engine faster-whisper:large-v3
```

| engine | notes |
|---|---|
| `whisper:<model>` | mlx-whisper. Default `large-v3-turbo`. Try plain `large-v3` on hard audio — turbo is distilled and gives up some accuracy |
| `faster-whisper:<model>` | CTranslate2. A different implementation of the same models, useful for telling a model problem from a runtime bug |
| `sarvam:<model>` | Saarika/Saaras, built for Indian accents and code-mixed speech. **Cloud — audio leaves the machine.** Needs `SARVAM_API_KEY`. Untested, written from docs |

`transcribe.py` depends only on the `Transcriber` interface in `engines.py`;
adding an engine is a class plus one registry line, with nothing downstream
touched. Two rules for any new engine:

- **It must return timestamped segments.** The entire two-stream attribution
  design merges by timestamp, so an engine returning one undifferentiated block
  of text is unusable unless it is chunked first — which is what the Sarvam
  backend does, since that API returns a whole-file transcript.
- **Declare `cloud = True` if audio leaves the machine.** `transcribe.py` warns
  loudly in that case. This project is local-first by premise, so that should be
  a deliberate, visible choice rather than a config default.

The hallucination filtering is deliberately outside the engines: it works off
waveform energy and the returned timestamps, so every backend gets it for free.

## Testing without a call

You cannot always summon a messy meeting. The closest available proxy:

```sh
./record.sh 300                                  # play real accented conversation
./.venv/bin/python transcribe.py recordings/<ts>
python3 simulate_call.py recordings/<ts>         # opus 24k, as a meeting codec would
./.venv/bin/python transcribe.py recordings/<ts>-call
```

Play a genuine multi-speaker conversation with natural code-switching — a
podcast or interview, not a scripted read — and talk over it yourself so the mic
stream carries real speech too. `simulate_call.py` then squeezes the system
stream through Opus at meeting bitrate, so you can measure what call
compression costs you. It degrades only the system stream, because in a real
call your own mic is recorded locally at full quality while the far end arrives
already encoded.

What this still does not reproduce: real-time packet loss, the client's noise
suppression and automatic gain control, and genuine crosstalk between the two
streams. It gets you most of the way, not all of it.

## Things that will bite

- **Never set `kAudioAggregateDeviceTapAutoStartKey`.** With that key set, the
  IOProc is never invoked at all when the default output device is Bluetooth —
  no error, no callbacks, a capture containing nothing. Measured on AirPods Pro:
  0 callbacks with the key vs ~188 in two seconds without it, every other
  setting held constant. `capture` prints an IOProc callback count in its `tap
  diagnostics` line, so this failure is visible rather than mysterious.

- **Wear headphones.** On speakers, the mic picks up the far end too and both
  streams contain everyone. `transcribe.py` drops the obvious echoes, but
  attribution degrades. Headphones keep the two-stream design honest.
- **`num_ctx`.** Set to 32768 in `enhance.py`. Ollama's default is far smaller
  and truncates the transcript with no error — you get a confident note about
  the first fifteen minutes of the meeting.
- **Whisper hallucinates in silence.** Each stream is silent whenever the other
  person is talking, so this is most of the audio, not an edge case.
  `transcribe.py` filters segments against the actual waveform energy rather
  than trusting `no_speech_prob` alone.
- **`--language en` translates, it does not transcribe.** This is the most
  dangerous behaviour in the whole pipeline. Forcing the English language token
  on Hindi-dominant speech makes Whisper emit fluent English *translation* — the
  actual words are gone, and nothing downstream can tell, because the output
  looks like an excellent transcript. Verified on a real recording: Hindi
  child-directed speech came back as clean English sentences.

  There is no setting that transcribes code-switched speech faithfully in Latin
  script. `en` translates the Hindi away, `hi` transliterates the English into
  Devanagari, `auto` picks one and flips mid-recording. That is a Whisper
  limitation, not a tuning problem.

  The design decision: **transcribe faithfully, normalise later.** Use
  `--language hi` for Hindi-dominant audio and let `enhance.py` render the note
  in English — it has the whole transcript plus your notes as context, so it
  normalises far better than a decoder working 30 seconds at a time. Default is
  `auto`; `transcribe.py` warns when you use `en`, and when Devanagari is present.

- **Repetition in a transcript is not automatically a hallucination.** A run of
  repeated phrases looks exactly like Whisper's best-known failure mode, and on
  this project it was misdiagnosed as one — the speaker had genuinely repeated
  himself, and the waveform showed a matching number of separate speech bursts.
  Check the audio before blaming the model. `condition_on_previous_text=False`
  is set regardless, since real loops do also happen on sparse streams.
- **RAM.** `gpt-oss:20b` is 13 GB of weights on a 32 GB machine. If enhancement
  crawls, stop Docker.
