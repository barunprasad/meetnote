//! speakrs — pyannote's pipeline reimplemented in Rust, with CoreML.
//!
//! **Provenance caveat.** The default bundle pairs pyannote's `segmentation-3.0`
//! (CNRS, France) with `wespeaker-voxceleb-resnet34` for embeddings, and
//! WeSpeaker is Chinese in origin — which the brief rules out. The escape hatch
//! is `ModelBundle::from_dir`: speakrs loads both models by filename from a
//! directory we choose, so the embedding half can be replaced with SpeechBrain
//! ECAPA-TDNN or NVIDIA TitaNet without touching this code, provided the
//! replacement exports compatible tensor shapes.
//!
//! `model_origin()` reports what is actually loaded so the constraint is
//! visible rather than assumed.

use std::path::PathBuf;
use std::sync::Mutex;

use speakrs::{AhcConfig, ExecutionMode, PipelineBuilder, PipelineConfig};

use crate::{Backend, Error, ModelOrigin, Result, Span};

pub struct Speakrs {
    /// The pipeline holds model state and is not `Sync`; one lock keeps the
    /// backend usable from the app's worker threads.
    pipeline: Mutex<speakrs::OwnedDiarizationPipeline>,
    origin: ModelOrigin,
}

impl Speakrs {
    /// Downloads the default bundle on first use.
    pub fn pretrained() -> Result<Speakrs> {
        Speakrs::tuned(default_config())
    }

    /// Same models, explicit clustering configuration — the knob that decides
    /// how many speakers come back.
    pub fn tuned(config: PipelineConfig) -> Result<Speakrs> {
        let builder = PipelineBuilder::from_pretrained(ExecutionMode::CoreMl)
            .map_err(|e| Error::Models(e.to_string()))?
            .pipeline(config);
        Speakrs::build(builder, DEFAULT_ORIGIN)
    }

    /// Loads models from a directory we control — the path used when the
    /// embedding model has been substituted.
    pub fn from_dir(dir: impl Into<PathBuf>, origin: ModelOrigin) -> Result<Speakrs> {
        Speakrs::build(PipelineBuilder::from_dir(dir, ExecutionMode::CoreMl), origin)
    }

    fn build(builder: PipelineBuilder, origin: ModelOrigin) -> Result<Speakrs> {
        let pipeline = builder.build().map_err(|e| Error::Models(e.to_string()))?;
        Ok(Speakrs { pipeline: Mutex::new(pipeline), origin })
    }
}

/// Measured on `fixtures/demo` — two known far-end speakers, eight turns —
/// with `cargo test -p meetnote-diarize --release -- --ignored`:
///
/// ```text
/// ahc_threshold   speakers   accuracy
///   0.60 (default)   4 / 2      93.5%
///   0.70             3 / 2      93.5%
///   0.80             2 / 2      99.1%   <- chosen
///   0.90             1 / 2      38.4%
/// ```
///
/// The window is narrow: one step further collapses both speakers into one and
/// accuracy falls off a cliff. And the fixture is TTS, whose voices are far more
/// consistent than real accented speech over a compressed call — so treat 0.8
/// as a starting point measured on a best case, and re-run the sweep against
/// real audio before trusting it. The benchmark takes five seconds.
pub fn default_config() -> PipelineConfig {
    PipelineConfig {
        ahc: AhcConfig { threshold: 0.8 },
        // Adjacent segments from the same speaker are one turn, not several.
        merge_gap: 0.5,
        speaker_keep_threshold: 0.02,
        ..PipelineConfig::default()
    }
}

/// What `from_pretrained` actually fetches.
pub const DEFAULT_ORIGIN: ModelOrigin = ModelOrigin {
    segmentation: "pyannote segmentation-3.0 (CNRS, France)",
    embedding: "wespeaker-voxceleb-resnet34 (WeSpeaker, China)",
};

/// The intended replacement once an ECAPA export is in place.
pub const ECAPA_ORIGIN: ModelOrigin = ModelOrigin {
    segmentation: "pyannote segmentation-3.0 (CNRS, France)",
    embedding: "SpeechBrain ECAPA-TDNN (Mila / Concordia)",
};

impl Backend for Speakrs {
    fn diarize(&self, samples: &[f32]) -> Result<Vec<Span>> {
        let mut pipeline = self.pipeline.lock().map_err(|e| Error::Failed(e.to_string()))?;
        let result = pipeline.run(samples).map_err(|e| Error::Failed(e.to_string()))?;
        Ok(result
            .segments
            .into_iter()
            .map(|segment| Span {
                start_secs: segment.start,
                end_secs: segment.end,
                speaker: segment.speaker,
            })
            .collect())
    }

    fn describe(&self) -> String {
        "speakrs · coreml".into()
    }

    fn model_origin(&self) -> ModelOrigin {
        self.origin.clone()
    }
}
