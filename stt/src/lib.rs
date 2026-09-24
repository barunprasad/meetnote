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
pub fn load_16k_mono(path: &Path) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();

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

/// Fraction of frames in a span that carry more than the floor.
fn speech_ratio(rms: &[f32], start: f64, end: f64) -> f32 {
    let lo = ((start * RATE as f64) as usize / FRAME).min(rms.len());
    let hi = (((end * RATE as f64) as usize).div_ceil(FRAME)).min(rms.len());
    if hi <= lo {
        return 0.0;
    }
    let above = rms[lo..hi].iter().filter(|&&r| r > FLOOR).count();
    above as f32 / (hi - lo) as f32
}

// ── transcription ──────────────────────────────────────────────────────────

/// Transcribes every stream and merges them onto one timeline.
pub fn transcribe(streams: &[Stream], options: &Options) -> Result<Vec<Turn>> {
    let context = WhisperContext::new_with_params(options.model, WhisperContextParameters::default())?;

    let mut segments: Vec<Turn> = Vec::new();
    for stream in streams {
        let audio = load_16k_mono(stream.path)?;
        if audio.iter().all(|s| s.abs() < 1e-4) {
            continue; // nothing was captured on this side
        }
        let rms = energy(&audio);

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(options.language);
        params.set_translate(false);
        // Sparse streams send this into repetition loops otherwise.
        params.set_no_context(true);
        params.set_print_progress(false);
        params.set_print_special(false);
        params.set_print_realtime(false);
        // Stops the model emitting sound-event tokens in the first place.
        params.set_suppress_nst(true);
        if let Some(vocab) = options.vocab {
            params.set_initial_prompt(vocab);
        }

        let mut state = context.create_state()?;
        state.full(params, &audio)?;

        for i in 0..state.full_n_segments() {
            let Some(segment) = state.get_segment(i) else { continue };
            let text = segment.to_str_lossy()?.trim().to_string();
            if text.is_empty() {
                continue;
            }
            if is_caption(&text) {
                continue;
            }

            // whisper.cpp reports centiseconds.
            let start = segment.start_timestamp() as f64 / 100.0;
            let end = segment.end_timestamp() as f64 / 100.0;
            let ratio = speech_ratio(&rms, start, end);

            // The model's own confidence, which it turns out whisper-rs
            // exposes per segment. Cheaper and better-founded than inferring
            // everything from waveform energy.
            if segment.no_speech_probability() > 0.6 && ratio < 0.35 {
                continue;
            }

            // Low energy alone is never grounds for dropping: an earlier
            // version judged each segment against its own peak, and audio
            // bleeding from the speakers raised the bar above the speaker's own
            // voice — deleting eleven real sentences. There must be positive
            // evidence of invention.
            let looks_invented = {
                let lowered = text.to_lowercase();
                let bare = lowered.trim_matches(|c: char| !c.is_alphanumeric());
                INVENTIONS.iter().any(|phrase| bare == *phrase || bare.starts_with(phrase))
            };
            if looks_invented && ratio < 0.35 {
                continue;
            }
            if ratio < 0.02 {
                continue;
            }

            segments.push(Turn {
                start_secs: start + stream.offset_secs,
                end_secs: end + stream.offset_secs,
                speaker: stream.speaker.to_string(),
                speaker_name: None,
                text,
            });
        }
    }

    segments.sort_by(|a, b| a.start_secs.partial_cmp(&b.start_secs).unwrap_or(std::cmp::Ordering::Equal));
    Ok(coalesce(drop_echo(segments)))
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
    fn quiet_regions_score_low_and_loud_ones_score_high() {
        let quiet = vec![0.0001_f32; RATE];
        let loud = vec![0.2_f32; RATE];
        assert!(speech_ratio(&energy(&quiet), 0.0, 1.0) < 0.05);
        assert!(speech_ratio(&energy(&loud), 0.0, 1.0) > 0.95);
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
