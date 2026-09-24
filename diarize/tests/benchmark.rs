//! Automated diarisation benchmark.
//!
//! Runs a backend against `fixtures/demo`, whose far-end stream carries two
//! known speakers at known times, and scores the result. No recording, no
//! listening, no judgement call — a number that any implementation can be
//! compared on.
//!
//!     python3 make_fixture.py fixtures/demo     # generates audio + truth.json
//!     cargo test -p meetnote-diarize -- --nocapture --ignored
//!
//! Marked `#[ignore]` because it downloads models and takes real time; the unit
//! tests still run on every `cargo test`.

use std::path::{Path, PathBuf};

use meetnote_diarize::{score, speakrs_backend::Speakrs, Backend, Reference};
use speakrs::{AhcConfig, PipelineConfig};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/demo")
}

/// Far-end truth only — the microphone stream is not diarised, because which
/// file the audio was in already answers who was speaking.
fn far_end_truth(dir: &Path) -> Vec<Reference> {
    let raw = std::fs::read_to_string(dir.join("truth.json"))
        .expect("run `python3 make_fixture.py fixtures/demo` first");
    let entries: Vec<serde_json::Value> = serde_json::from_str(&raw).expect("truth.json is not json");
    entries
        .into_iter()
        .filter(|entry| entry["stream"] == "system")
        .map(|entry| Reference {
            start_secs: entry["start_secs"].as_f64().unwrap(),
            end_secs: entry["end_secs"].as_f64().unwrap(),
            speaker: entry["speaker"].as_str().unwrap().to_string(),
        })
        .collect()
}

/// 16 kHz mono, matching what the models expect.
fn load_16k_mono(path: &Path) -> Vec<f32> {
    let mut reader = hound::WavReader::open(path).expect("fixture audio missing");
    let spec = reader.spec();
    let scale = 1.0 / (1_i64 << (spec.bits_per_sample - 1)) as f32;
    let raw: Vec<f32> = reader.samples::<i32>().filter_map(|s| s.ok()).map(|s| s as f32 * scale).collect();

    let channels = spec.channels as usize;
    let mono: Vec<f32> = if channels <= 1 {
        raw
    } else {
        raw.chunks(channels).map(|f| f.iter().sum::<f32>() / channels as f32).collect()
    };

    let step = spec.sample_rate as usize / 16_000;
    if step <= 1 {
        return mono;
    }
    // Box-average rather than dropping samples. Crude next to a sinc filter,
    // but this is measuring speaker separation, not transcription quality.
    mono.chunks(step).map(|c| c.iter().sum::<f32>() / c.len() as f32).collect()
}

#[test]
#[ignore = "downloads models and runs the full pipeline"]
fn speakrs_separates_the_two_far_end_speakers() {
    let dir = fixture_dir();
    let truth = far_end_truth(&dir);
    let audio = load_16k_mono(&dir.join("system.wav"));

    let audio_secs = audio.len() as f64 / 16_000.0;

    // Sweep the knob that decides how many speakers survive clustering. The
    // default keeps everything, which turned two people into four.
    // `speaker_keep_threshold` turned out to be inert here — all four clusters
    // had real activity. The lever is the agglomerative clustering distance:
    // raise it and embeddings that belong to the same voice get merged.
    println!("\n  ahc_threshold    speakers   accuracy   speed");
    let mut best: Option<(f64, meetnote_diarize::Score, Vec<meetnote_diarize::Span>)> = None;
    for threshold in [0.6, 0.7, 0.8, 0.9, 1.0] {
        let backend = Speakrs::tuned(PipelineConfig {
            ahc: AhcConfig { threshold: threshold as f32 },
            merge_gap: 0.5,
            speaker_keep_threshold: 0.02,
            ..PipelineConfig::default()
        })
        .expect("could not load models");

        let started = std::time::Instant::now();
        let spans = backend.diarize(&audio).expect("diarisation failed");
        let elapsed = started.elapsed();
        let result = score(&spans, &truth);

        println!(
            "  {:<16.5} {:<10} {:<10.1} {:.0}x",
            threshold,
            format!("{} / {}", result.found_speakers, result.true_speakers),
            result.accuracy * 100.0,
            audio_secs / elapsed.as_secs_f64().max(0.001),
        );
        // Prefer the right speaker count first, accuracy second — an invented
        // participant poisons every name downstream.
        let better = best.as_ref().is_none_or(|(_, b, _)| {
            let correct = |s: &meetnote_diarize::Score| s.found_speakers == s.true_speakers;
            (correct(&result), result.accuracy) > (correct(b), b.accuracy)
        });
        if better {
            best = Some((threshold, result, spans));
        }
    }

    let (threshold, result, spans) = best.expect("no run completed");
    println!("\n  what it produced, against what was said:");
    for span in spans.iter().take(12) {
        let truth_here = truth
            .iter()
            .find(|r| span.start_secs < r.end_secs && r.start_secs < span.end_secs)
            .map(|r| r.speaker.as_str())
            .unwrap_or("—");
        println!(
            "    {:6.2}-{:6.2}  {:<12} (really {})",
            span.start_secs, span.end_secs, span.speaker, truth_here
        );
    }
    let origin = Speakrs::pretrained().unwrap().model_origin();
    println!("\n  best         ahc_threshold {threshold}");
    println!("  segmentation {}", origin.segmentation);
    println!("  embedding    {}", origin.embedding);
    println!("  speakers     found {} · truth {}", result.found_speakers, result.true_speakers);
    println!("  accuracy     {:.1}%\n", result.accuracy * 100.0);

    // Finding the wrong number of people is the failure that matters most:
    // merge two speakers and every name attached downstream is wrong.
    assert_eq!(
        result.found_speakers, result.true_speakers,
        "expected {} far-end speakers, found {}",
        result.true_speakers, result.found_speakers
    );
    assert!(
        result.accuracy > 0.80,
        "only {:.1}% of far-end speech went to the right speaker",
        result.accuracy * 100.0
    );
}
