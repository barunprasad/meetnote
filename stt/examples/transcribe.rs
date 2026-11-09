//! Runs the app's transcription on a recording folder, outside the app.
//!
//! ```sh
//! cargo run --release --example transcribe -- <model.bin> <recording dir> \
//!     [system offset secs] [true system rate]
//! ```
//!
//! For checking filter changes against real audio without rebuilding and
//! reinstalling the bundle. Prints one line per turn. `MEETNOTE_VOCAB` sets the
//! decoding hint the app would pass; `MEETNOTE_STT_TRACE=1` prints every
//! segment with its evidence on stderr.

use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [model, dir, rest @ ..] = args.as_slice() else {
        eprintln!("usage: transcribe <model.bin> <recording dir> [system offset] [true system rate]");
        std::process::exit(2);
    };
    let offset: f64 = rest.first().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let rate: Option<u32> = rest.get(1).and_then(|v| v.parse().ok());

    let dir = PathBuf::from(dir);
    let (mic, system) = (dir.join("mic.wav"), dir.join("system.wav"));
    let streams = [
        meetnote_stt::Stream { path: &mic, speaker: "ME", offset_secs: 0.0, sample_rate: None },
        meetnote_stt::Stream { path: &system, speaker: "THEM", offset_secs: offset, sample_rate: rate },
    ];
    let model = PathBuf::from(model);
    let vocab = std::env::var("MEETNOTE_VOCAB").ok();
    let turns = meetnote_stt::transcribe(
        &streams,
        &meetnote_stt::Options { model: &model, language: None, vocab: vocab.as_deref() },
    )
    .expect("transcription failed");

    for turn in turns {
        let secs = turn.start_secs as u64;
        println!("[{:02}:{:02}:{:02}] {}: {}", secs / 3600, secs / 60 % 60, secs % 60, turn.speaker, turn.text);
    }
}
