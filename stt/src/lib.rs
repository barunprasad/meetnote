//! Two-stream transcription.
//!
//! Each stream is transcribed on its own and the results are merged onto one
//! timeline using the host-clock offsets recorded at capture. Speaker
//! attribution therefore comes from *which file the audio was in* — a fact
//! about the recording rather than a diarisation model's guess.
//!
//! whisper.cpp runs in-process via `whisper-rs`. It is a C library, so there is
//! no reason to wrap it in a separate process.

use std::path::Path;

use meetnote_core::Turn;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Whisper(#[from] whisper_rs::WhisperError),
    #[error(transparent)]
    Wav(#[from] hound::Error),
    #[error("resampling failed: {0}")]
    Resample(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Whisper's required input rate.
const RATE: usize = 16_000;

/// 20 ms analysis frames, matching the filter this was ported from.
const FRAME: usize = RATE / 50;

/// Roughly −50 dBFS. Below this, a region is empty enough that any words
/// Whisper produced for it were invented.
const FLOOR: f32 = 3.2e-3;

/// Phrases Whisper emits into silence. Each stream here is silent whenever the
/// other person is talking, so this is most of the audio rather than an edge
/// case.
/// A segment that is nothing but a caption — `*sad music*`, `[Music]`,
/// `(laughter)` — is a subtitle habit whisper picked up in training, not
/// something anyone said. Real speech is never wrapped this way, so these go
/// whatever the energy behind them: quiet room noise is exactly when whisper
/// reaches for one.
fn is_caption(text: &str) -> bool {
    let t = text.trim();
    let pairs = [('*', '*'), ('[', ']'), ('(', ')'), ('♪', '♪'), ('<', '>')];
    let Some(first) = t.chars().next() else { return false };
    let Some(last) = t.chars().last() else { return false };
    if !pairs.iter().any(|(open, close)| first == *open && last == *close) {
        return false;
    }
    // Only when the whole segment is the caption: "*sighs* yes, agreed" is a
    // real line with an artefact attached, and dropping it would lose speech.
    let inner = &t[first.len_utf8()..t.len() - last.len_utf8()];
    !inner.contains(['*', '[', ']', '(', ')'])
}

const INVENTIONS: &[&str] = &[
    "thank you", "thanks for watching", "thanks", "please subscribe", "subscribe",
    "bye", "okay", "ok", "you", "amara.org", "www.",
];

pub struct Stream<'a> {
    pub path: &'a Path,
    /// `ME` or `THEM` — which file this is decides the speaker.
    pub speaker: &'a str,
    /// Host-clock offset from the earliest stream. The merge depends on it.
    pub offset_secs: f64,
    /// The rate the audio was really recorded at, when the wav header is known
    /// to be wrong (see `meetnote_core::Stream::true_rate`). `None` trusts it.
    pub sample_rate: Option<u32>,
}

pub struct Options<'a> {
    pub model: &'a Path,
    /// `None` auto-detects. **Never pass `Some("en")` blindly**: forcing English
    /// on Hindi-dominant speech makes Whisper *translate* rather than
    /// transcribe, and the output is fluent enough that nothing downstream can
    /// tell the real words are gone.
    pub language: Option<&'a str>,
    /// Names and jargon to bias decoding toward — the words that otherwise come
    /// back mangled.
    pub vocab: Option<&'a str>,
}

// ── audio ──────────────────────────────────────────────────────────────────

/// Mono f32 at 16 kHz, downmixing and resampling as needed. Public because
/// diarisation needs exactly the same input.
///
/// `true_rate` overrides the header. Getting this wrong is not a quality issue:
/// audio read at twice its real rate is unintelligible, and Whisper fills it
/// with confident invented sentences rather than leaving it blank.
pub fn load_16k_mono(path: &Path, true_rate: Option<u32>) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)?;
    let mut spec = reader.spec();
    if let Some(rate) = true_rate {
        spec.sample_rate = rate;
    }

    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1_i64 << (spec.bits_per_sample - 1)) as f32;
            reader.samples::<i32>().filter_map(|s| s.ok()).map(|s| s as f32 * scale).collect()
        }
        hound::SampleFormat::Float => reader.samples::<f32>().filter_map(|s| s.ok()).collect(),
    };

    // Downmix: the system tap is stereo, the mic mono.
    let channels = spec.channels as usize;
    let mono: Vec<f32> = if channels <= 1 {
        raw
    } else {
        raw.chunks(channels).map(|f| f.iter().sum::<f32>() / channels as f32).collect()
    };

    if spec.sample_rate as usize == RATE {
        return Ok(mono);
    }
    resample(&mono, spec.sample_rate as usize)
}

/// Proper sinc resampling rather than dropping samples — decimating 48 kHz to
/// 16 kHz without a filter aliases speech back into the band Whisper reads.
fn resample(input: &[f32], from: usize) -> Result<Vec<f32>> {
    use rubato::{Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction};

    let params = SincInterpolationParameters {
        sinc_len: 128,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 128,
        window: WindowFunction::BlackmanHarris2,
    };
    let mut resampler =
        SincFixedIn::<f32>::new(RATE as f64 / from as f64, 2.0, params, input.len().max(1), 1)
            .map_err(|e| Error::Resample(e.to_string()))?;
    let out = resampler
        .process(&[input.to_vec()], None)
        .map_err(|e| Error::Resample(e.to_string()))?;
    Ok(out.into_iter().next().unwrap_or_default())
}

/// RMS per 20 ms frame.
fn energy(samples: &[f32]) -> Vec<f32> {
    samples
        .chunks_exact(FRAME)
        .map(|frame| (frame.iter().map(|s| s * s).sum::<f32>() / FRAME as f32).sqrt())
        .collect()
}

/// The level a stream must clear to count as someone speaking: 10 dB over its
/// own background, and never below the absolute floor.
///
/// The fixed −50 dBFS bar alone was too close to a laptop microphone's room
/// noise (−54 to −58 dBFS on the 29 Oct interview): the noise crossed it often
/// enough that whisper's inventions over a quiet microphone scored as speech.
/// The background is the level the stream sits at a fifth of the time; digital
/// silence, which the system stream is whenever nobody on the far end talks,
/// is left out so it cannot drag the estimate to nothing.
fn noise_floor(rms: &[f32]) -> f32 {
    let mut live: Vec<f32> = rms.iter().copied().filter(|r| *r > 1e-5).collect();
    if live.len() < 50 {
        return FLOOR;
    }
    let fifth = live.len() / 5;
    let (_, background, _) = live.select_nth_unstable_by(fifth, |a, b| a.total_cmp(b));
    FLOOR.max(*background * 3.16)
}

/// Fraction of frames in a span that carry more than the floor.
fn speech_ratio(rms: &[f32], floor: f32, start: f64, end: f64) -> f32 {
    let (lo, hi) = frames_of(rms, start, end);
    if hi <= lo {
        return 0.0;
    }
    let above = rms[lo..hi].iter().filter(|&&r| r > floor).count();
    above as f32 / (hi - lo) as f32
}

/// The loudest 100 ms in a span, as RMS.
fn peak(rms: &[f32], start: f64, end: f64) -> f32 {
    let (lo, hi) = frames_of(rms, start, end);
    if hi <= lo {
        return 0.0;
    }
    rms[lo..hi]
        .windows(5.min(hi - lo))
        .map(|w| w.iter().sum::<f32>() / w.len() as f32)
        .fold(0.0, f32::max)
}

/// The frames covering a span of seconds.
fn frames_of(rms: &[f32], start: f64, end: f64) -> (usize, usize) {
    let lo = ((start.max(0.0) * RATE as f64) as usize / FRAME).min(rms.len());
    let hi = (((end.max(0.0) * RATE as f64) as usize).div_ceil(FRAME)).min(rms.len());
    (lo, hi)
}

/// Stretches of a stream worth decoding, as sample ranges, each short enough
/// for whisper to take in one window.
///
/// Decoding a forty-minute stream in one call is how the 29 Oct interview lost
/// whole minutes: whisper invents a line over a quiet patch, its timestamps
/// run ahead, and the seek skips real speech after it — two minutes of the
/// candidate's answer became "So we can use it to be changed." Decoded chunk by
/// chunk, a bad window costs only itself, and the long silences each stream has
/// while the other person talks never reach the decoder at all.
///
/// The bar is the same absolute floor as the invention check: anything above
/// −50 dBFS is kept, bleed and background included. This decides what whisper
/// listens to, not what counts as speech — erring wide only costs time.
fn speech_chunks(rms: &[f32]) -> Vec<(usize, usize)> {
    const FRAMES_PER_SEC: usize = RATE / FRAME;
    const BRIDGE: usize = FRAMES_PER_SEC; // pauses under a second stay inside a region
    const PAD: usize = FRAMES_PER_SEC * 3 / 10; // 0.3 s either side, for word onsets
    const MIN_REGION: usize = FRAMES_PER_SEC * 15 / 100;
    const MAX_CHUNK: usize = FRAMES_PER_SEC * 28; // whisper's window is 30 s
    // Every chunk costs a full 30 s encoder pass however short it is, so
    // regions are packed together across pauses of a few seconds; only the
    // long silences while the other side holds the floor are left out.
    const JOIN_GAP: usize = FRAMES_PER_SEC * 8;

    // Runs of frames above the floor, with short pauses bridged.
    let mut regions: Vec<(usize, usize)> = Vec::new();
    for (i, level) in rms.iter().enumerate() {
        if *level <= FLOOR {
            continue;
        }
        match regions.last_mut() {
            Some((_, end)) if i <= *end + BRIDGE => *end = i + 1,
            _ => regions.push((i, i + 1)),
        }
    }
    regions.retain(|(start, end)| end - start >= MIN_REGION);

    // Neighbouring regions share a chunk when they fit, so a back-and-forth
    // of short replies is decoded with its context rather than word by word.
    let mut chunks: Vec<(usize, usize)> = Vec::new();
    for (start, end) in regions {
        let (start, end) = (start.saturating_sub(PAD), (end + PAD).min(rms.len()));
        match chunks.last_mut() {
            Some((chunk_start, chunk_end)) if start <= *chunk_end + JOIN_GAP && end - *chunk_start <= MAX_CHUNK => {
                *chunk_end = end.max(*chunk_end)
            }
            _ => chunks.push((start, end)),
        }
    }

    // Anyone talking for longer than a window is cut at a pause in its last
    // eight seconds, so the cut lands between words.
    let mut bounded = Vec::new();
    for (mut start, end) in chunks {
        while end - start > MAX_CHUNK {
            let cut = quiet_cut(rms, FLOOR, start + MAX_CHUNK - FRAMES_PER_SEC * 8..start + MAX_CHUNK);
            bounded.push((start, cut));
            start = cut;
        }
        bounded.push((start, end));
    }
    bounded.into_iter().map(|(start, end)| (start * FRAME, (end * FRAME))).collect()
}

/// Where to cut a stretch of frames: the middle of its longest run below the
/// floor, or failing any, its quietest frame.
///
/// The quietest single frame alone can be the closure inside a word — a "t" —
/// and a cut there leaves half a word on each side of it.
fn quiet_cut(rms: &[f32], floor: f32, range: std::ops::Range<usize>) -> usize {
    let mut best: Option<(usize, usize)> = None; // (length, middle)
    let mut run_start = None;
    for frame in range.start..=range.end {
        match (frame < range.end && rms[frame] <= floor, run_start) {
            (true, None) => run_start = Some(frame),
            (false, Some(from)) => {
                if best.is_none_or(|(length, _)| frame - from > length) {
                    best = Some((frame - from, (from + frame) / 2));
                }
                run_start = None;
            }
            _ => {}
        }
    }
    match best {
        Some((_, middle)) => middle,
        None => range
            .clone()
            .min_by(|a, b| rms[*a].total_cmp(&rms[*b]))
            .unwrap_or(range.end),
    }
}

// ── transcription ──────────────────────────────────────────────────────────

/// One decoded line, with the evidence the filters judge it on.
struct Line {
    turn: Turn,
    /// The span whisper gave, on the meeting timeline, before tightening.
    span: (f64, f64),
    /// Index into the streams, for the checks that compare one with another.
    stream: usize,
    /// Fraction of the line's span above its stream's noise floor.
    ratio: f32,
    /// The loudest 100 ms in the span, as a multiple of the noise floor.
    peak: f32,
    /// Mean log-probability of its words: how sure the decoder was.
    logprob: f32,
}

/// A stream's levels, kept for the cross-stream checks.
struct Levels {
    rms: Vec<f32>,
    floor: f32,
    offset_secs: f64,
}

impl Levels {
    /// Fraction of a span on the meeting timeline where this stream is above
    /// its floor.
    fn active(&self, start: f64, end: f64) -> f32 {
        speech_ratio(&self.rms, self.floor, start - self.offset_secs, end - self.offset_secs)
    }
}

/// Set `MEETNOTE_STT_TRACE=1` to print every line with its evidence and the
/// verdict, for tuning the filters against a real recording.
fn tracing() -> bool {
    std::env::var_os("MEETNOTE_STT_TRACE").is_some()
}

/// Transcribes every stream and merges them onto one timeline.
pub fn transcribe(streams: &[Stream], options: &Options) -> Result<Vec<Turn>> {
    let context = WhisperContext::new_with_params(options.model, WhisperContextParameters::default())?;
    let eot = context.token_eot();
    let trace = tracing();

    let mut lines: Vec<Line> = Vec::new();
    let mut levels: Vec<Option<Levels>> = Vec::new();
    for (index, stream) in streams.iter().enumerate() {
        let audio = load_16k_mono(stream.path, stream.sample_rate)?;
        if audio.iter().all(|s| s.abs() < 1e-4) {
            levels.push(None);
            continue; // nothing was captured on this side
        }
        let rms = energy(&audio);
        let floor = noise_floor(&rms);

        let params = |language: Option<&'static str>| {
            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            params.set_language(language.or(options.language));
            params.set_translate(false);
            // Sparse streams send this into repetition loops otherwise.
            params.set_no_context(true);
            params.set_print_progress(false);
            params.set_print_special(false);
            params.set_print_realtime(false);
            // Stops the model emitting sound-event tokens in the first place.
            params.set_suppress_nst(true);
            // Per-word times, for splitting a segment that spans a pause.
            params.set_token_timestamps(true);
            if let Some(vocab) = options.vocab {
                params.set_initial_prompt(vocab);
            }
            params
        };

        let mut state = context.create_state()?;
        let chunks = speech_chunks(&rms);
        if trace {
            eprintln!(
                "{}: floor {:.1} dBFS, {} chunks",
                stream.speaker,
                20.0 * floor.log10(),
                chunks.len()
            );
        }

        // One language per stream, detected on its longest chunk. Left to each
        // chunk, a two-second reply is detected at p≈0.6 and can flip to
        // another language mid-meeting; the single-pass decode this replaced
        // detected once, and that behaviour is kept.
        let language = match (options.language, chunks.iter().max_by_key(|(lo, hi)| hi - lo)) {
            (None, Some((lo, hi))) => {
                state.full(params(None), &audio[*lo..*hi])?;
                whisper_rs::get_lang_str(state.full_lang_id_from_state())
            }
            _ => None,
        };

        // The last line kept on this stream, for catching loops that span
        // segments and chunks.
        let mut previous: Option<(String, f64)> = None;
        let mut queue: std::collections::VecDeque<(usize, usize)> = chunks.into();
        while let Some((lo, hi)) = queue.pop_front() {
            state.full(params(language), &audio[lo..hi])?;
            let chunk_start = lo as f64 / RATE as f64;
            let chunk_end = hi as f64 / RATE as f64;
            if trace {
                eprintln!("  chunk {chunk_start:.2}–{chunk_end:.2}: {} segments", state.full_n_segments());
            }

            // A window of plain speech that comes back empty: whisper gave up
            // on it rather than heard nothing. On the 29 Oct interview this was
            // 27 s of you describing the team — "we hire a thousand people,
            // right? No, not like that" — gone without a trace. Halves, cut at
            // a pause, decode.
            if state.full_n_segments() == 0
                && hi - lo > RATE * 6
                && speech_ratio(&rms, floor, chunk_start, chunk_end) > 0.3
            {
                let (first, last) = (lo / FRAME, hi / FRAME);
                let quarter = (last - first) / 4;
                let cut = quiet_cut(&rms, floor, first + quarter..last - quarter) * FRAME;
                queue.push_front((cut, hi));
                queue.push_front((lo, cut));
                continue;
            }
            for i in 0..state.full_n_segments() {
                let Some(segment) = state.get_segment(i) else { continue };
                let no_speech = segment.no_speech_probability();

                // whisper.cpp reports centiseconds from the start of the chunk,
                // and happily past its end: the window is padded to 30 s, and
                // a last segment would run on into the padding.
                let at = |centis: i64| (chunk_start + centis as f64 / 100.0).clamp(chunk_start, chunk_end);
                let span = (at(segment.start_timestamp()), at(segment.end_timestamp()));
                let mut words: Vec<Word> = Vec::new();
                for k in 0..segment.n_tokens() {
                    let Some(token) = segment.get_token(k) else { continue };
                    let data = token.token_data();
                    if data.id >= eot {
                        continue; // timestamps and other control tokens
                    }
                    words.push(Word {
                        at: (at(data.t0) + at(data.t1)) / 2.0,
                        bytes: token.to_bytes()?.to_vec(),
                        logprob: data.plog,
                    });
                }

                for (start, end, words) in split_at_pauses(&rms, floor, span, words) {
                    let bytes: Vec<u8> = words.iter().flat_map(|w| w.bytes.iter().copied()).collect();
                    let text = collapse_repeats(tidy(&String::from_utf8_lossy(&bytes)).as_str());
                    if text.is_empty() {
                        continue;
                    }
                    let ratio = speech_ratio(&rms, floor, start, end);
                    let peak = peak(&rms, start, end) / floor;
                    let logprob = words.iter().map(|w| w.logprob).sum::<f32>() / words.len().max(1) as f32;

                    let verdict = judge(&text, ratio, no_speech, logprob);
                    if trace {
                        eprintln!(
                            "  {:>8.2} {:>8.2} {} ratio {ratio:.2} peak {:+.0} dB lp {logprob:.2} nsp {no_speech:.2} {} | {text}",
                            start + stream.offset_secs,
                            end + stream.offset_secs,
                            stream.speaker,
                            20.0 * peak.log10(),
                            verdict.unwrap_or("kept"),
                        );
                    }
                    if verdict.is_some() {
                        continue;
                    }

                    // The same line again, word for word, within a minute: the
                    // decoder looping on its own output over a quiet stretch,
                    // which `no_context` reduces but does not stop. It loops in
                    // single words too — "Thank you." thirteen segments running —
                    // and a repeated "Yeah." collapsed to one loses nothing
                    // anyone said.
                    let key = normalise(&text);
                    if let Some((last, at)) = &previous {
                        if *last == key && start - at < 60.0 {
                            continue;
                        }
                    }
                    previous = Some((key, start));

                    // The span whisper gave runs from the timestamp before the
                    // words to the one after; the voice itself is narrower, and
                    // sorting on the wider span put answers ahead of the
                    // questions they answered.
                    let (span_start, span_end) = (start, end);
                    let (start, end) = tighten(&rms, floor, start, end);
                    lines.push(Line {
                        turn: Turn {
                            start_secs: start + stream.offset_secs,
                            end_secs: end + stream.offset_secs,
                            speaker: stream.speaker.to_string(),
                            speaker_name: None,
                            text,
                        },
                        span: (span_start + stream.offset_secs, span_end + stream.offset_secs),
                        stream: index,
                        ratio,
                        peak,
                        logprob,
                    });
                }
            }
        }
        levels.push(Some(Levels { rms, floor, offset_secs: stream.offset_secs }));
    }

    let mut segments: Vec<Turn> = drop_quiet_mic_lines(lines, streams, &levels, trace);
    segments.sort_by(|a, b| a.start_secs.partial_cmp(&b.start_secs).unwrap_or(std::cmp::Ordering::Equal));
    let mut turns = coalesce(drop_echo(segments));
    // Joining segments can line up a loop that no single segment showed.
    for turn in &mut turns {
        turn.text = collapse_repeats(&turn.text);
    }
    Ok(turns)
}

/// A decoded token: its text, when it was said, and how sure the decoder was.
struct Word {
    at: f64,
    bytes: Vec<u8>,
    logprob: f32,
}

/// Trims a line and the punctuation a split can leave at its start — ", so
/// you're joining" — capitalising what follows.
fn tidy(text: &str) -> String {
    let text = text.trim().trim_start_matches(|c: char| matches!(c, ',' | ';' | ':' | '.' | '-') || c.is_whitespace());
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Why a line should be dropped on its own evidence, or `None` to keep it.
///
/// Low energy alone is never grounds for dropping: an earlier version judged
/// each segment against its own peak, and audio bleeding from the speakers
/// raised the bar above the speaker's own voice — deleting eleven real
/// sentences. There must be positive evidence of invention as well.
fn judge(text: &str, ratio: f32, no_speech: f32, logprob: f32) -> Option<&'static str> {
    if normalise(text).is_empty() {
        return Some("no words"); // a split can leave a lone "." or "-"
    }
    if is_caption(text) {
        return Some("caption");
    }
    if is_filler(text) {
        return Some("filler");
    }
    if ratio < 0.02 {
        return Some("silent");
    }
    // The model's own confidence that there was no speech.
    if no_speech > 0.6 && ratio < 0.35 {
        return Some("no speech");
    }
    if is_stock_phrase(text) && ratio < 0.35 {
        return Some("stock phrase");
    }
    // Words the decoder itself barely believed, over audio that is mostly
    // not voice: on the 29 Oct interview, "This is the definition of wine."
    // over a quiet microphone.
    if logprob < LOW_CONFIDENCE && ratio < 0.5 {
        return Some("unsure");
    }
    None
}

/// Mean token log-probability below which the decoder was guessing.
const LOW_CONFIDENCE: f32 = -0.8;

/// The whole line is phrases whisper emits into silence, and nothing else.
///
/// Matched exactly, sentence by sentence. An earlier version matched by
/// prefix, and "Okay. So you're joining on which date?" went as an invented
/// "okay" — the reply to it then read as volunteered.
fn is_stock_phrase(text: &str) -> bool {
    let sentences: Vec<String> = text
        .split(['.', '?', '!', ','])
        .map(normalise)
        .filter(|s| !s.is_empty())
        .collect();
    !sentences.is_empty() && sentences.iter().all(|s| INVENTIONS.contains(&s.as_str()))
}

/// Drops microphone lines over a quiet microphone while the other side talks.
///
/// On headphones the microphone hears only you, so while the far end holds
/// the floor it carries room noise, and whisper turns room noise into
/// sentences. A real interjection is loud on your own microphone even when it
/// is short; an invented one sits just above the noise.
fn drop_quiet_mic_lines(lines: Vec<Line>, streams: &[Stream], levels: &[Option<Levels>], trace: bool) -> Vec<Turn> {
    let far: Vec<&Levels> = streams
        .iter()
        .zip(levels)
        .filter(|(stream, _)| stream.speaker != "ME")
        .filter_map(|(_, levels)| levels.as_ref())
        .collect();
    lines
        .into_iter()
        .filter(|line| {
            if streams[line.stream].speaker != "ME" {
                return true;
            }
            let (start, end) = line.span;
            let far_end = far.iter().map(|l| l.active(start, end)).fold(0.0_f32, f32::max);
            let quiet = line.peak < QUIET_PEAK && (line.ratio < 0.5 || line.logprob < LOW_CONFIDENCE);
            let drop = far_end > 0.5 && quiet;
            if trace && line.peak < QUIET_PEAK {
                let verdict = if drop { "dropped" } else { "kept" };
                eprintln!("  quiet mic line {verdict}, far end {far_end:.2}: {start:.2} {}", line.turn.text);
            }
            !drop
        })
        .map(|line| line.turn)
        .collect()
}

/// How far above its noise floor a microphone line must peak to count as
/// someone speaking into it: 20 dB.
const QUIET_PEAK: f32 = 10.0;

/// Splits a segment at pauses of 1.5 s or more, using the words' times.
///
/// Stretches up to eight seconds apart share a chunk (see `speech_chunks`), and
/// whisper will run "Okay." and the next question, asked after the other
/// person's answer, into one segment — stamped with the time of the first.
/// Sorted by that time, the question then came before the answer.
fn split_at_pauses(rms: &[f32], floor: f32, (start, end): (f64, f64), words: Vec<Word>) -> Vec<(f64, f64, Vec<Word>)> {
    // Long enough that a hesitation inside a sentence is not a pause: at
    // 0.8 s, "Any other feature flag we have worked on?" came out in three.
    const PAUSE: usize = 75; // frames: 1.5 s
    let (lo, hi) = frames_of(rms, start, end);

    // Midpoints of the silent runs long enough to be a pause.
    let mut cuts: Vec<f64> = Vec::new();
    let mut run_start = None;
    for frame in lo..=hi {
        let quiet = frame < hi && rms[frame] <= floor;
        match (quiet, run_start) {
            (true, None) => run_start = Some(frame),
            (false, Some(from)) => {
                if frame - from >= PAUSE && from > lo && frame < hi {
                    cuts.push((from + frame) as f64 / 2.0 * FRAME as f64 / RATE as f64);
                }
                run_start = None;
            }
            _ => {}
        }
    }

    let mut bounds = vec![start];
    bounds.extend(cuts);
    bounds.push(end);
    let mut words = words.into_iter().peekable();
    let mut pieces = Vec::new();
    for pair in bounds.windows(2) {
        let mut piece = Vec::new();
        while let Some(word) = words.next_if(|w| w.at < pair[1] || pair[1] == end) {
            piece.push(word);
        }
        if !piece.is_empty() {
            pieces.push((pair[0], pair[1], piece));
        }
    }
    pieces
}

/// Narrows a span to where its stream is actually above the floor, with a
/// tenth of a second either side for soft onsets.
fn tighten(rms: &[f32], floor: f32, start: f64, end: f64) -> (f64, f64) {
    let (lo, hi) = frames_of(rms, start, end);
    let loud = |frame: &usize| rms[*frame] > floor;
    let (Some(first), Some(last)) = ((lo..hi).find(loud), (lo..hi).rev().find(loud)) else {
        return (start, end);
    };
    let secs = |frame: usize| (frame * FRAME) as f64 / RATE as f64;
    ((secs(first) - 0.1).max(start), (secs(last + 1) + 0.1).min(end))
}

/// Lowercase words only, for comparing lines regardless of punctuation.
fn normalise(text: &str) -> String {
    text.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Collapses a sentence repeated back to back into one copy.
///
/// Whisper's characteristic failure on audio it cannot make sense of is a loop:
/// "And then, we can build that." five times in one segment. Real speech does
/// repeat short words ("Okay. Okay."), which is harmless to collapse, so the
/// rule is simply that consecutive identical sentences become one.
fn collapse_repeats(text: &str) -> String {
    let mut sentences: Vec<&str> = Vec::new();
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if matches!(c, '.' | '?' | '!' | '…' | '。') {
            let end = i + c.len_utf8();
            // "..." is one terminator, not three empty sentences; and a stop
            // not followed by a space is a decimal or a version ("20.0").
            if text[end..].starts_with(['.', '?', '!'])
                || text[end..].chars().next().is_some_and(|next| !next.is_whitespace())
            {
                continue;
            }
            sentences.push(&text[start..end]);
            start = end;
        }
    }
    sentences.push(&text[start..]);

    let mut kept: Vec<&str> = Vec::new();
    for sentence in sentences.into_iter().map(str::trim).filter(|s| !s.is_empty()) {
        let same = kept.last().is_some_and(|last| {
            let key = normalise(sentence);
            !key.is_empty() && normalise(last) == key
        });
        if !same {
            kept.push(sentence);
        }
    }
    kept.join(" ")
}

/// Nothing but hesitation sounds. Whisper emits a lone "um" over long stretches
/// of the other person talking, and a twelve-second "uh" carries nothing a
/// reader or the note could use. Backchannel like "mm-hmm" or "yeah" is kept:
/// it is a real response.
fn is_filler(text: &str) -> bool {
    const FILLERS: &[&str] = &["um", "umm", "uh", "uhh", "uh-huh-uh", "er", "erm", "ah", "eh", "hm", "hmm"];
    let words = normalise(text);
    !words.is_empty() && words.split(' ').all(|w| FILLERS.contains(&w))
}

/// Removes ME segments that are the far end bleeding into the microphone.
/// Only happens on speakers rather than headphones, and the text-similarity
/// test misses partial overlaps — this is a mitigation, not a fix.
fn drop_echo(segments: Vec<Turn>) -> Vec<Turn> {
    let them: Vec<(f64, f64, String)> = segments
        .iter()
        .filter(|t| t.speaker != "ME")
        .map(|t| (t.start_secs, t.end_secs, t.text.to_lowercase()))
        .collect();

    segments
        .into_iter()
        .filter(|turn| {
            if turn.speaker != "ME" {
                return true;
            }
            let mine = turn.text.to_lowercase();
            !them.iter().any(|(start, end, text)| {
                turn.start_secs < *end && *start < turn.end_secs && similar(&mine, text) > 0.8
            })
        })
        .collect()
}

/// Word overlap, enough to catch an echo of the same sentence.
fn similar(a: &str, b: &str) -> f32 {
    let left: Vec<&str> = a.split_whitespace().collect();
    if left.is_empty() {
        return 0.0;
    }
    let shared = left.iter().filter(|word| b.contains(*word)).count();
    shared as f32 / left.len() as f32
}

/// Joins consecutive same-speaker segments into readable turns.
fn coalesce(segments: Vec<Turn>) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    for segment in segments {
        match turns.last_mut() {
            Some(last) if last.speaker == segment.speaker && segment.start_secs - last.end_secs <= 2.0 => {
                last.text.push(' ');
                last.text.push_str(&segment.text);
                last.end_secs = segment.end_secs;
            }
            _ => turns.push(segment),
        }
    }
    turns
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtitle_captions_are_dropped_but_speech_with_an_artefact_is_kept() {
        for caption in ["*sad music*", "[Music]", "(laughter)", "♪♪", "*coughs*"] {
            assert!(is_caption(caption), "{caption} should be dropped");
        }
        for speech in [
            "*sighs* yes, agreed",
            "we keep the keys for 72 hours",
            "(a) is the option we picked, (b) is not",
            "",
        ] {
            assert!(!is_caption(speech), "{speech} is speech and must survive");
        }
    }

    #[test]
    fn a_decoder_loop_collapses_to_one_sentence() {
        // Both straight from the 29 Oct interview: the first is what double-speed
        // audio became, the second a loop over a quiet microphone.
        assert_eq!(
            collapse_repeats("And then, we can build that. And then, we can build that. And then, we can build that."),
            "And then, we can build that."
        );
        assert_eq!(
            collapse_repeats("What will be your response to this? What will be your response to this?"),
            "What will be your response to this?"
        );
        assert_eq!(collapse_repeats("It's... It's... It's... going to be fun"), "It's... going to be fun");
    }

    #[test]
    fn ordinary_speech_survives_the_loop_filter() {
        for line in [
            "Yes, yes. All of this version should be replicated here.",
            "Okay. And how do you feel micro-frontends is working out?",
            "We use Nx 20.0. It works.",
            "yeah yeah yeah yeah okay",
        ] {
            assert_eq!(collapse_repeats(line), line);
        }
    }

    #[test]
    fn lone_hesitations_go_but_responses_stay() {
        for filler in ["um", "Uh.", "uh, um", "Hmm..."] {
            assert!(is_filler(filler), "{filler}");
        }
        for response in ["Mm-hmm.", "Yeah.", "um, I think so", "Okay"] {
            assert!(!is_filler(response), "{response}");
        }
    }

    /// RMS frames from (seconds, loud?) spans.
    fn frames(spans: &[(f64, bool)]) -> Vec<f32> {
        spans
            .iter()
            .flat_map(|(secs, loud)| {
                std::iter::repeat_n(if *loud { 0.1 } else { 0.0001 }, (*secs * 50.0) as usize)
            })
            .collect()
    }

    #[test]
    fn long_silences_never_reach_the_decoder() {
        // Ten minutes of the other person talking, then a reply.
        let rms = frames(&[(600.0, false), (4.0, true), (30.0, false)]);
        let chunks = speech_chunks(&rms);
        assert_eq!(chunks.len(), 1);
        let (start, end) = chunks[0];
        let secs = |s: usize| s as f64 / RATE as f64;
        assert!((secs(start) - 599.7).abs() < 0.05 && (secs(end) - 604.3).abs() < 0.05, "{chunks:?}");
    }

    #[test]
    fn every_chunk_fits_one_whisper_window() {
        // A four-minute monologue with breaths, then short replies.
        let mut spans = Vec::new();
        for _ in 0..60 {
            spans.extend([(3.6, true), (0.4, false)]);
        }
        spans.extend([(5.0, false), (0.5, true), (1.5, false), (0.6, true)]);
        let chunks = speech_chunks(&frames(&spans));
        assert!(chunks.iter().all(|(s, e)| (e - s) as f64 / RATE as f64 <= 28.0), "{chunks:?}");
        // Nothing loud is left out, and nothing is decoded twice.
        assert!(chunks.windows(2).all(|w| w[0].1 <= w[1].0));
        let covered: usize = chunks.iter().map(|(s, e)| e - s).sum();
        assert!(covered as f64 / RATE as f64 > 240.0);
    }

    #[test]
    fn a_click_is_not_a_chunk_but_a_short_yes_is() {
        assert!(speech_chunks(&frames(&[(5.0, false), (0.06, true), (5.0, false)])).is_empty());
        assert_eq!(speech_chunks(&frames(&[(5.0, false), (0.3, true), (5.0, false)])).len(), 1);
    }

    #[test]
    fn quiet_regions_score_low_and_loud_ones_score_high() {
        let quiet = vec![0.0001_f32; RATE];
        let loud = vec![0.2_f32; RATE];
        assert!(speech_ratio(&energy(&quiet), FLOOR, 0.0, 1.0) < 0.05);
        assert!(speech_ratio(&energy(&loud), FLOOR, 0.0, 1.0) > 0.95);
    }

    #[test]
    fn consecutive_same_speaker_segments_become_one_turn() {
        let turn = |start: f64, end: f64, speaker: &str, text: &str| Turn {
            start_secs: start, end_secs: end, speaker: speaker.into(),
            speaker_name: None, text: text.into(),
        };
        let merged = coalesce(vec![
            turn(0.0, 1.0, "ME", "first"),
            turn(1.5, 2.0, "ME", "second"),
            turn(2.2, 3.0, "THEM", "reply"),
        ]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].text, "first second");
        assert_eq!(merged[1].speaker, "THEM");
    }

    #[test]
    fn a_long_gap_starts_a_new_turn() {
        let turn = |start: f64, end: f64| Turn {
            start_secs: start, end_secs: end, speaker: "ME".into(),
            speaker_name: None, text: "x".into(),
        };
        assert_eq!(coalesce(vec![turn(0.0, 1.0), turn(9.0, 10.0)]).len(), 2);
    }

    #[test]
    fn speaker_echo_is_dropped_but_genuine_speech_is_kept() {
        let them = Turn {
            start_secs: 5.0, end_secs: 8.0, speaker: "THEM".into(), speaker_name: None,
            text: "we cannot get closer than this to it".into(),
        };
        let echo = Turn {
            start_secs: 5.2, end_secs: 8.1, speaker: "ME".into(), speaker_name: None,
            text: "we cannot get closer than this".into(),
        };
        let mine = Turn {
            start_secs: 5.1, end_secs: 7.0, speaker: "ME".into(), speaker_name: None,
            text: "let me check the staging deploy".into(),
        };
        let kept = drop_echo(vec![them, echo, mine]);
        assert_eq!(kept.len(), 2, "the echo should go, the real sentence should stay");
        assert!(kept.iter().any(|t| t.text.contains("staging deploy")));
    }

    #[test]
    fn the_two_streams_interleave_by_time_not_by_file() {
        // Sorting is what puts a reply after the remark it answers, even though
        // the two came from separate recordings.
        let seg = |start: f64, speaker: &str, text: &str| Turn {
            start_secs: start, end_secs: start + 1.0, speaker: speaker.into(),
            speaker_name: None, text: text.into(),
        };
        let mut segments = vec![
            seg(6.0, "ME", "roughly twelve thousand"),
            seg(2.0, "ME", "forty percent done"),
            seg(4.0, "THEM", "how many duplicates?"),
        ];
        segments.sort_by(|a, b| a.start_secs.partial_cmp(&b.start_secs).unwrap());
        let order: Vec<&str> = segments.iter().map(|t| t.speaker.as_str()).collect();
        assert_eq!(order, ["ME", "THEM", "ME"]);
    }
}
