//! Splitting the far-end stream into individual speakers.
//!
//! Two-stream capture already answers the hard half of diarisation — whether a
//! voice is the person recording or someone else is a fact about *which file*
//! the audio is in. This only has to split `THEM` into `THEM-1`, `THEM-2`,
//! which is a strictly easier problem than partitioning a mixed recording.
//!
//! The implementation sits behind a trait for the same reason the STT engine
//! and the LLM provider do, and with an added reason here: the models carry a
//! provenance constraint (see `Backend::model_origin`), so being able to swap
//! them is a requirement rather than a nicety.

use meetnote_core::Turn;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("diarisation failed: {0}")]
    Failed(String),
    #[error("models are not available: {0}")]
    Models(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// One stretch of audio attributed to one speaker within a single stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub start_secs: f64,
    pub end_secs: f64,
    /// Opaque within-stream label. Meaningless across streams or meetings until
    /// something (calendar attendees, a voice the user names) resolves it.
    pub speaker: String,
}

/// The seam. Anything that can label spans can be measured by `score`.
pub trait Backend: Send + Sync {
    /// 16 kHz mono samples in, labelled spans out.
    fn diarize(&self, samples: &[f32]) -> Result<Vec<Span>>;

    fn describe(&self) -> String;

    /// Where the weights come from. The brief rules out Chinese-origin models,
    /// and the default pyannote pipeline ships a WeSpeaker embedding, so this
    /// is a real decision surface rather than trivia.
    fn model_origin(&self) -> ModelOrigin;
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelOrigin {
    pub segmentation: &'static str,
    pub embedding: &'static str,
}

// ── applying spans to a transcript ─────────────────────────────────────────

/// Relabels `THEM` turns with the speaker who was talking at that moment.
///
/// `ME` is never touched: the microphone stream is definitionally the person
/// recording, and letting a clustering model second-guess that would trade a
/// certainty for a probability.
pub fn apply(turns: &mut [Turn], spans: &[Span]) {
    for turn in turns.iter_mut().filter(|t| t.speaker == "THEM") {
        if let Some(span) = dominant(spans, turn.start_secs, turn.end_secs) {
            turn.speaker = format!("THEM-{}", label_index(&span.speaker));
        }
    }
}

/// The speaker holding the most of this turn, rather than whoever happens to
/// start it — a short interjection at the boundary should not claim the line.
fn dominant<'a>(spans: &'a [Span], start: f64, end: f64) -> Option<&'a Span> {
    spans
        .iter()
        .map(|span| {
            let overlap = (span.end_secs.min(end) - span.start_secs.max(start)).max(0.0);
            (span, overlap)
        })
        .filter(|(_, overlap)| *overlap > 0.0)
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(span, _)| span)
}

/// `SPEAKER_00` → `1`, so the transcript reads `THEM-1` rather than `THEM-0`.
fn label_index(label: &str) -> usize {
    label
        .rsplit('_')
        .next()
        .and_then(|n| n.parse::<usize>().ok())
        .map(|n| n + 1)
        .unwrap_or(1)
}

// ── measuring a backend ────────────────────────────────────────────────────

/// A labelled reference, for scoring.
#[derive(Debug, Clone)]
pub struct Reference {
    pub start_secs: f64,
    pub end_secs: f64,
    pub speaker: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Score {
    /// Seconds attributed to the right speaker, as a fraction of speech time.
    pub accuracy: f64,
    /// How many speakers it found against how many were really there. Getting
    /// this wrong is worse than a few misattributed seconds: a merged pair of
    /// speakers makes every downstream name wrong.
    pub found_speakers: usize,
    pub true_speakers: usize,
}

/// Scores predicted spans against a labelled reference by overlap in time.
///
/// Deliberately not the standard Diarisation Error Rate: DER needs an optimal
/// label mapping and careful handling of overlapped speech, and we want a
/// number two implementations can be compared on, not a published metric.
pub fn score(predicted: &[Span], reference: &[Reference]) -> Score {
    let mapping = best_mapping(predicted, reference);

    let mut correct = 0.0;
    let mut total = 0.0;
    for truth in reference {
        total += truth.end_secs - truth.start_secs;
        for span in predicted {
            let overlap =
                (span.end_secs.min(truth.end_secs) - span.start_secs.max(truth.start_secs)).max(0.0);
            if overlap > 0.0 && mapping.get(&span.speaker) == Some(&truth.speaker) {
                correct += overlap;
            }
        }
    }

    Score {
        accuracy: if total > 0.0 { correct / total } else { 0.0 },
        found_speakers: unique(predicted.iter().map(|s| s.speaker.clone())),
        true_speakers: unique(reference.iter().map(|r| r.speaker.clone())),
    }
}

/// Predicted labels are arbitrary, so each is mapped to whichever true speaker
/// it overlaps most. Without this, a perfect result scores zero purely because
/// it called someone `SPEAKER_01` instead of `SPEAKER_00`.
fn best_mapping(
    predicted: &[Span],
    reference: &[Reference],
) -> std::collections::HashMap<String, String> {
    let mut mapping = std::collections::HashMap::new();
    for label in predicted.iter().map(|s| &s.speaker).collect::<std::collections::BTreeSet<_>>() {
        let mut best: Option<(String, f64)> = None;
        for truth in reference {
            let overlap: f64 = predicted
                .iter()
                .filter(|s| &s.speaker == label)
                .map(|s| {
                    (s.end_secs.min(truth.end_secs) - s.start_secs.max(truth.start_secs)).max(0.0)
                })
                .sum();
            if overlap > best.as_ref().map_or(0.0, |(_, o)| *o) {
                best = Some((truth.speaker.clone(), overlap));
            }
        }
        if let Some((speaker, _)) = best {
            mapping.insert(label.clone(), speaker);
        }
    }
    mapping
}

fn unique(labels: impl Iterator<Item = String>) -> usize {
    labels.collect::<std::collections::BTreeSet<_>>().len()
}

// ── speakrs ────────────────────────────────────────────────────────────────

pub mod speakrs_backend;

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: f64, end: f64, speaker: &str) -> Span {
        Span { start_secs: start, end_secs: end, speaker: speaker.into() }
    }
    fn reference(start: f64, end: f64, speaker: &str) -> Reference {
        Reference { start_secs: start, end_secs: end, speaker: speaker.into() }
    }
    fn turn(start: f64, end: f64, speaker: &str) -> Turn {
        Turn {
            start_secs: start, end_secs: end, speaker: speaker.into(),
            speaker_name: None, text: "…".into(),
        }
    }

    #[test]
    fn the_microphone_stream_is_never_relabelled() {
        // ME is a fact about which file the audio was in. A clustering model
        // must not be allowed to overrule it.
        let mut turns = vec![turn(0.0, 5.0, "ME"), turn(5.0, 10.0, "THEM")];
        apply(&mut turns, &[span(0.0, 10.0, "SPEAKER_03")]);
        assert_eq!(turns[0].speaker, "ME");
        assert_eq!(turns[1].speaker, "THEM-4");
    }

    #[test]
    fn a_turn_goes_to_whoever_held_most_of_it() {
        // A one-second interjection at the start must not claim a ten-second turn.
        let mut turns = vec![turn(0.0, 10.0, "THEM")];
        apply(&mut turns, &[span(0.0, 1.0, "SPEAKER_00"), span(1.0, 10.0, "SPEAKER_01")]);
        assert_eq!(turns[0].speaker, "THEM-2");
    }

    #[test]
    fn arbitrary_labels_do_not_penalise_a_correct_result() {
        // Perfect diarisation that happens to number its speakers the other way
        // round is still perfect.
        let predicted = vec![span(0.0, 5.0, "SPEAKER_01"), span(5.0, 10.0, "SPEAKER_00")];
        let truth = vec![reference(0.0, 5.0, "tara"), reference(5.0, 10.0, "aman")];
        let scored = score(&predicted, &truth);
        assert!((scored.accuracy - 1.0).abs() < 1e-9, "got {}", scored.accuracy);
        assert_eq!(scored.found_speakers, 2);
    }

    #[test]
    fn merging_two_speakers_into_one_scores_badly() {
        // The failure that matters most: if two people are treated as one,
        // every name attached downstream is wrong.
        let predicted = vec![span(0.0, 10.0, "SPEAKER_00")];
        let truth = vec![reference(0.0, 5.0, "tara"), reference(5.0, 10.0, "aman")];
        let scored = score(&predicted, &truth);
        assert_eq!(scored.found_speakers, 1);
        assert_eq!(scored.true_speakers, 2);
        assert!(scored.accuracy < 0.6, "half the time is attributed to the wrong person");
    }

    #[test]
    fn turns_with_no_overlapping_span_keep_their_original_label() {
        let mut turns = vec![turn(50.0, 60.0, "THEM")];
        apply(&mut turns, &[span(0.0, 10.0, "SPEAKER_00")]);
        assert_eq!(turns[0].speaker, "THEM", "guessing would be worse than leaving it");
    }
}
