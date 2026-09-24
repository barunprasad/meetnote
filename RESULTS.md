# Spike results

Answers to the four questions in `BRIEF.md`. Everything here was measured on
this machine; where a question is only partly answered, that is stated rather
than smoothed over.

**Headline: three of the four questions are answered and the architecture holds.
The fourth — the project-killing one — is answered only on synthetic audio and
still needs one real call.**

## 1. Does the Core Audio tap reliably capture the far end? — *yes, confirmed*

Confirmed on this machine with audio playing through AirPods Pro:

```
mic    : 30.0s  (+0.000s offset)  peak -24.6 dBFS
system : 30.0s  (+0.003s offset)  peak  -1.4 dBFS
tap diagnostics: 2814 IOProc callbacks, 0 dropped buffers
```

No bot, no virtual audio device, no Screen Recording permission, no recording
indicator. The captured system stream transcribed cleanly. Two real bugs had to
be fixed first, both invisible from a headless shell — see below.

The API path works end to end. `AudioHardwareCreateProcessTap` succeeds, the
private aggregate device is created and runs, the IOProc delivers buffers at
48 kHz stereo, files close cleanly, and no aggregate device is leaked
afterwards (checked via `system_profiler`). `--list` enumerates processes and
correctly flags which are playing output.

What is *not* verified is that real call audio lands in the file. Every capture
run so far produced digital silence — exactly 0, not a low noise floor. That is
TCC withholding audio: macOS does not error on a denied tap, it hands back a
working tap that yields zeros. Confirmed environmental rather than a code bug by
recording the same microphone with ffmpeg, which also produced exact zeros.
Agent-spawned shells cannot receive the consent prompt.

A real bug surfaced on the first user-run test, which reported `NO FRAMES AT
ALL` for the system stream: **`kAudioAggregateDeviceTapAutoStartKey` breaks the
tap when the default output device is Bluetooth.** With that key set the IOProc
is never invoked — no error, no callbacks, an empty capture. Isolated by holding
every other setting constant across seven aggregate-device configurations:

| sub-device | drift | autostart | IOProc callbacks in 2 s |
|---|---|---|---|
| output device | true | **yes** | **0** |
| none | false | **yes** | **0** |
| output device | true | no | 188 |
| output device | false | no | 188 |
| none | false | no | 186 |

The key was my addition and is now removed. With AirPods Pro as the default
output the tap went from 0 to ~470 callbacks over five seconds, delivering
interleaved stereo float32 with no dropped buffers. `capture` now reports its
IOProc callback count, so this class of failure is visible rather than silent.

A second, more structural bug followed: **a bare CLI binary cannot own a TCC
grant.** macOS attributes its requests to the responsible process — the
launching terminal — and Warp never surfaces the prompt: `AVCaptureDevice`
reports `notDetermined`, `requestAccess` returns false immediately, nothing is
drawn on screen, and the capture is silent with no error anywhere.

Fixed by shipping a signed `meetnote-capture.app` and launching it via `open`
(`record.sh`), so launchd owns the process and it becomes responsible for
itself. The prompt then appears with the `Info.plist` usage strings. This is why
the AudioCap reference is an app rather than a CLI tool, which was not obvious
until this failed.

**Confirmed working:** after granting the prompt, `mic.wav` came back at
mean −58.9 dBFS / peak −40.3 dBFS — real audio, first non-silent capture on this
machine. Microphone and Audio Recording are separate TCC services and prompt
separately; system audio still needs its own grant.

**Needs you:** `./record.sh 30` with something playing, approving the Audio
Recording prompt. Non-silent `system.wav` closes this question.

## 2. Is the transcript usable on accented, code-switched audio? — *tested on real speech; the danger is not what we expected*

**The headline finding of the spike: `--language en` translates rather than
transcribes, silently.**

Verified on a real 90-second recording of Hindi-dominant Hinglish (the user
talking to a child). With `--language en`, Whisper returned fluent English —
"Where is your helicopter? It is flying in the sky." — which is a *translation*
of what was said, not a transcript. The same audio at `--language hi` returns
`तेजस क्या कर रहा है`, `फ्लाइग इन दे स्काई`.

This is the worst failure mode available: the output is confident, fluent and
looks like an excellent transcript, so nothing downstream can detect that the
actual words were discarded. It was the default for most of this spike.

There is no setting that gives faithful code-switched text in Latin script:

| setting | behaviour on Hindi-dominant Hinglish |
|---|---|
| `en` | translates the Hindi away |
| `hi` | faithful, but transliterates English into Devanagari (`फ्लाइग इन दे स्काई`) |
| `auto` | detects one language and flips mid-recording |

**Resolution: transcribe faithfully, normalise in the LLM.** `--language hi`
keeps the real words; `enhance.py` renders the note in English with the whole
transcript plus the user's notes as context, which is far more information than
a decoder working 30 seconds at a time. Verified end to end — `gpt-oss:20b` read
the Devanagari transcript, produced a correct English note, kept one section per
fragment, marked the unsupported fragment unverified, and ignored Whisper's
repetition loops rather than trying to render them.

Also observed on real audio: the name `Tejas` degraded to `Aegeus`/`Aegis` after
the first minute. `--vocab "Tejas"` is the lever for that and was not tried.

The repeated phrases in that transcript (`आई` ×11, `we will go` ×7) were first
recorded here as decoder loops. **That was wrong.** Checking the waveform found
16 distinct speech bursts across the 11 s of the first and 41 across the 31 s of
the second — the speaker really was repeating himself, which is ordinary when
talking to a small child. Whisper was being faithful. Worth keeping as a
methodological warning: repetition in a transcript looks exactly like Whisper's
best-known failure mode, and the only way to tell them apart is to go back to
the audio.

**Caveat on representativeness:** this sample was Hindi-dominant, child-directed
speech with long pauses and genuine repetition. A work meeting is usually
English-dominant with Hindi interjections, where `en` may behave very
differently — possibly fine. That test has not been run.

### Earlier result on synthetic audio (retained for contrast)

On Indian-English TTS (`Rishi`/`Tara`/`Aman`) with deliberate Hinglish, Whisper
large-v3-turbo produced a clean transcript. Errors clustered in a very specific
and, for this use case, forgiving place:

| survived intact | mangled |
|---|---|
| names — Priya, Rohan, Meera | `Haan` → "huh" / "Ha!" |
| numbers — 40%, 12,000, v2.4, 3 weeks | `Theek hai` → "Okay" |
| dates — Friday, Wednesday, Tuesdays | `Thoda detail do na` → "Thora detail doona" |
| jargon — idempotency, webhook, Kafka, escalation path, tribal knowledge | `Postgres` → "postgaz" |
| the decisions and commitments themselves | `on-call` → "uncall" |

The errors are almost entirely **Hinglish discourse markers** — "yes", "fine",
"give me detail" — which carry near-zero information for a meeting note. Every
semantically load-bearing token survived. That is the best failure mode this
could have had.

`--vocab` is a real mitigation: passing `Postgres` fixed "postgaz" outright. It
is not reliable for everything — `on-call` was in the same list and "uncall"
survived — and it over-capitalised `Matlab` to `MATLAB`.

### Distilled turbo vs undistilled large-v3

The theory was that `large-v3-turbo` is distilled and distillation costs most on
hard audio, so plain `large-v3` should do better on Hinglish. Scored against the
fixture's known ground truth with `score_fixture.py`:

| group | large-v3-turbo | large-v3-8bit |
|---|---|---|
| names | **3/3** | 2/3 |
| numbers | 4/4 | 4/4 |
| jargon | 5/7 | **6/7** |
| **hinglish** | **1/4** | **1/4** |
| content | **3/4** | 2/4 |
| **total** | **16/22** | 15/22 |

**The theory did not hold.** Turbo is marginally ahead, and more importantly the
Hinglish score is *identical* — 1/4 for both, with only `Matlab` surviving in
either. Moving to the undistilled model bought nothing on the exact axis it was
supposed to help. Turbo stays the default: equal or better, smaller, faster.

Two honest confounds. The 8-bit quantisation is not a clean isolation of
distillation, so this compares "8-bit large-v3" to "fp16 turbo" rather than
undistilled to distilled; fp16 `large-v3` (3.08 GB) would settle it. And the
audio is still TTS — macOS `en_IN` voices may not pronounce *Haan* or *Theek
hai* the way a person does, so a 1/4 on four tokens is weak evidence either way.

What it does suggest: if Hinglish discourse markers matter, another Whisper
variant is unlikely to be the fix. The lever is either `--vocab` biasing or a
different model family, which is what the `sarvam` backend is there to test.

The 8-bit model also produced coarser segment boundaries, merging speech across
a silence gap into the previous turn. Text quality was in some ways better
(punctuation, capitalisation, "on-call" instead of "uncall") but timestamp
precision is what the two-stream merge depends on, so that is a real cost.

**The caveat that matters:** this is synthetic speech. It has no crosstalk, no
room noise, no compression artefacts, no overlapping speakers, and far more
regular prosody than a real call. The brief explicitly warned against judging
this on a clean read-aloud sample, and TTS is cleaner still. **Treat this as
"no red flag yet", not as an answer.**

Speed: 106 s of audio across both streams in ~10 s wall clock, ≈20× realtime.
An hour-long meeting should transcribe in about three minutes.

## 3. Does the two-stream merge produce clean attribution? — *yes, confirmed on real simultaneous audio*

Tested with both streams live at once: the user talking work-style while a video
played. Attribution was correct throughout — the user's speech in ME, the video
in THEM, no diarisation model:

```
[00:00:25] ME:   We just need to make sure everything is running and healthy…
[00:00:23] THEM: …top speed of 1,200 miles an hour, an operational ceiling of 50,000 feet
```

Two things this run caught that nothing earlier did.

**The energy filter was deleting real speech.** Eleven of the user's fourteen
utterances were dropped. The filter derived a "noise floor" from the stream's
20th-percentile frame energy, but audio bleeding from the speakers raised that
floor to −30.8 dBFS — above the speaker's own voice — so ordinary sentences
scored as noise. `no_speech_prob` was `0.00` on every one; Whisper knew they were
speech and the waveform check overrode it.

The design was wrong in principle, not just mistuned. The filter's only job is
catching text invented over near-silence, not separating speech from
background. The adaptive term may now only ever *lower* the bar, and low energy
alone is no longer grounds for dropping — there must be positive evidence of
invention. Keeping a stray hallucination is cheap because the enhancement step
ignores it; deleting something that was said is unrecoverable and invisible.
After the fix: 14/14 of the user's utterances kept, the earlier phantom
`ME: Thank you.` still correctly dropped, fixture unchanged.

**Echo is real and the echo filter is not enough.** On speakers, the video leaked
into the mic and one ME turn contains `we can't get close to 100 million pounds`
— the video's words attributed to the user. The text-similarity echo filter
missed it because the overlap was partial. Headphones remain load-bearing.

### On synthetic audio

100% correct on the fixture: every ME turn came from the mic file, every THEM
turn from the tap file, with no diarisation model anywhere. Merged timestamps
landed within ~1 s of ground truth across the whole 106 s, using the host-clock
offsets `capture.swift` records at first buffer. Consecutive same-speaker
segments coalesce into readable turns.

Two caveats found while building it:

- **Headphones are load-bearing.** On speakers the mic captures the far end too
  and both files contain everyone. `transcribe.py` drops ME segments that
  overlap a THEM segment with near-identical text, but that is a mitigation, not
  a fix. The design assumes headphones.
- **Silence hallucination is structural here, not an edge case.** Each stream is
  silent whenever the other person talks, and Whisper invents text over silence.
  On the first real capture — 30 s of room tone on the mic, no speech at all —
  Whisper produced `[00:00:10] ME: Thank you.` A phantom turn attributed to ME
  is worse than a missing one, because `enhance.py` treats the transcript as
  evidence and will faithfully expand a fabricated line.

  The fixture could not have caught this: its silence is digital zeros, while
  real silence is a noise floor. The original filter measured each segment
  against its own peak, so room tone looked like quiet speech and passed. It now
  anchors to the stream's noise floor (20th percentile of frame RMS, ×4, with a
  −50 dBFS absolute floor). The phantom turn is dropped, the fixture is
  unchanged, and `transcribe.py` reports what percentage of each stream sits
  above the floor — 0% vs 65% on that recording, which reads at a glance.

## 4. Does `gpt-oss:20b` expand notes, or drift into generic summary? — *it expands, with the prompt doing the work*

It holds the discipline well. The fixture gives ~25 s of transcript to an on-call
rotation discussion that appears nowhere in the notes; the model excluded it in
every run while expanding the five noted fragments in order under the user's own
wording, and returned the unsupported fragment marked unverified rather than
dropping it or inventing support. Numbers and attributions carried through
correctly. ~60 s per run, most of it reasoning tokens.

This took two prompt revisions, and both failures are worth keeping in mind
because they will recur:

1. **The first version was extractive.** It copied speech verbatim, filler and
   all, and repeated itself. Fixed by instructing it to write settled statements
   of fact rather than replay dialogue.
2. **`--action-items` reopened the hole.** It listed a commitment made during the
   exact topic the user had ignored. Any section that is not explicitly scoped to
   the fragments becomes a whole-meeting summary — the generic-summary failure
   does not arrive head-on, it arrives through the side door.

`num_ctx: 32768` is set explicitly. `enhance.py` warns when input approaches it
instead of letting Ollama truncate silently.

## Architecture change: the STT engine is pluggable

Originally `transcribe.py` called `mlx_whisper.transcribe` inline, which meant
the component the brief identifies as most likely to fail was also the one
hardest to replace. That is now behind a `Transcriber` interface in
`engines.py`, selected with `--engine`.

This does not answer question 2 — you still have to find out whether any engine
handles Hinglish. What it changes is the cost of asking: the same recording can
be run through several engines and compared directly, instead of the answer
being a rewrite.

Three backends: `whisper` (mlx, default), `faster-whisper` (CTranslate2, a
different implementation of the same models), and `sarvam` (cloud, built for
Indian code-mixed speech, untested — written from the API docs).

Two constraints any new engine has to meet, both learned from the design rather
than imposed on it:

- **Timestamped segments are mandatory.** Speaker attribution comes entirely
  from merging two streams on one timeline, so a whole-file transcript is
  useless without chunking. The Sarvam backend chunks at 30 s because its API
  returns exactly that.
- **`cloud = True` when audio leaves the machine**, which `transcribe.py`
  warns about at runtime. Local-only is the project's premise, so giving it up
  should be visible rather than a default buried in config.

Hallucination filtering stays outside the engines — it works off waveform energy
and timestamps, so every backend inherits it. `load_audio` uses ffmpeg directly
rather than any engine's loader, so a cloud engine works with no local STT
package installed.

## State on pause — start here tomorrow

Everything below works and is verified unless marked otherwise. Nothing is
half-edited; the pipeline runs end to end on `fixtures/demo`.

**Permissions are granted and persist.** Microphone and Audio Recording are both
approved for `meetnote-capture.app`. Use `./record.sh` (not `./capture`) if a
prompt is ever needed again — a bare CLI binary cannot own a TCC grant.

**The one open question is #2**, and it needs real accented, code-switched
audio. Two ways in, in order of cost:

1. **A model comparison, no call needed.** `large-v3-turbo` is distilled, and
   distillation costs most on hard audio. The fixture already contains Hinglish
   with known ground truth, and turbo currently gets it wrong:

   | said | turbo heard |
   |---|---|
   | Haan | "huh" |
   | Thoda detail do na | "Thoradite" |
   | Theek hai | "Okay" |
   | Postgres | "postgaz" (fixed by `--vocab`) |

   ```sh
   ./compare_engines.py fixtures/demo whisper:large-v3-turbo whisper:large-v3-8bit
   ```

   **This was interrupted mid-download.** `whisper-large-v3-mlx-8bit` is 1.71 GB
   and was ~1% in; Hugging Face resumes from the partial file, so just re-run.
   8-bit large-v3 needs ~2 GB resident — less than the fp16 turbo already in use.

2. **A real call**, which is still the only thing that settles it. Headphones on,
   10+ minutes. `simulate_call.py` can then show what call-grade Opus costs on
   top.

**Watch the machine, not the models.** At pause: 0 GB free, 8.8 GB of 10 GB swap
used, 49 Docker containers. Whisper is irrelevant at ~2 GB, but `enhance.py`
loads 13 GB — stop Docker before judging that step's speed.

**Untested code, flagged honestly:**
- `SarvamCloud` in `engines.py` — written from the API docs, never run. Needs
  `SARVAM_API_KEY`. It also uploads audio, which breaks the local-only premise;
  it exists to quantify what staying local costs, not as a default.
- `faster-whisper` installs and registers as available, but has not completed a
  full transcription run.

## Recommended next step

One real call — messy, accented, 10+ minutes, headphones on — closes questions 1
and 2 together. Nothing else is worth building until that transcript exists,
because everything downstream is worthless if it is poisoned.

If Whisper does fail on real audio, the fallback is not Sarvam's open weights:
their ASR (Saarika/Saaras) is API-only and Ollama has no speech pipeline at all.
The real decision would be whether to accept a cloud ASR API and give up
fully-local transcription. Sarvam's *text* models (`sarvam-m`, `sarvam-30b`, both
GGUF, Apache 2.0, non-Chinese) are a plausible A/B against `gpt-oss:20b` for the
enhancement step once a real code-switched transcript exists to test with.
