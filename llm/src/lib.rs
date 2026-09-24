//! Turning sparse live notes into a full note.
//!
//! Ollama is the default, not a commitment — the provider sits behind a trait
//! for the same reason the STT engine does: it is the component most likely to
//! be swapped, so it is the one part that should not be wired in by hand.

pub mod prompt;

use std::io::{BufRead, BufReader};

use meetnote_core::{NoteFragment, Turn};
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot reach {0} — is it running?")]
    Unreachable(String),
    #[error("model `{0}` is not available")]
    MissingModel(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// The seam. `enhance` depends on this and nothing else.
pub trait LlmProvider: Send + Sync {
    /// Streams the note as it is written, so the UI can show progress on a run
    /// that takes a minute or more.
    fn generate(&self, system: &str, user: &str, on_token: &mut dyn FnMut(&str)) -> Result<String>;

    /// Usable input tokens. Ollama's is a per-request setting we choose; a
    /// cloud model's is fixed. The caller warns before silently overflowing it.
    fn context_window(&self) -> usize;

    /// True when the transcript leaves this machine. Surfaced in the UI at the
    /// point of use — local-only is the premise, and giving it up should be
    /// visible rather than a setting someone picked once and forgot.
    fn is_cloud(&self) -> bool;

    fn describe(&self) -> String;
}

// ── ollama ─────────────────────────────────────────────────────────────────

pub struct Ollama {
    base_url: String,
    model: String,
    num_ctx: usize,
}

impl Ollama {
    /// An hour of meeting is 12–18k tokens. Ollama's default context is far
    /// smaller and truncates the transcript **with no error** — you get a
    /// confident note about the first fifteen minutes. This must be set on
    /// every request.
    pub const NUM_CTX: usize = 32_768;

    pub fn new(model: impl Into<String>) -> Ollama {
        Ollama {
            base_url: "http://localhost:11434".into(),
            model: model.into(),
            num_ctx: Ollama::NUM_CTX,
        }
    }

    /// Models actually pulled on this machine — asked rather than hardcoded, so
    /// the list never drifts from reality.
    pub fn installed(base_url: &str) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct Tags {
            models: Vec<Model>,
        }
        #[derive(Deserialize)]
        struct Model {
            name: String,
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()?;
        let tags: Tags = client
            .get(format!("{base_url}/api/tags"))
            .send()
            .map_err(|_| Error::Unreachable(base_url.to_string()))?
            .json()?;
        Ok(tags.models.into_iter().map(|m| m.name).collect())
    }
}

#[derive(Deserialize)]
struct ChatChunk {
    message: Option<ChatMessage>,
    #[serde(default)]
    done: bool,
}

#[derive(Deserialize)]
struct ChatMessage {
    #[serde(default)]
    content: String,
}

impl LlmProvider for Ollama {
    fn generate(&self, system: &str, user: &str, on_token: &mut dyn FnMut(&str)) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
            "stream": true,
            "options": { "num_ctx": self.num_ctx, "temperature": 0.3, "top_p": 0.9 },
        });

        let client = reqwest::blocking::Client::builder()
            // A 20B model on a busy machine is slow; a default timeout would
            // abandon a note that was nearly finished.
            .timeout(std::time::Duration::from_secs(1800))
            .build()?;
        let response = client
            .post(format!("{}/api/chat", self.base_url))
            .json(&body)
            .send()
            .map_err(|_| Error::Unreachable(self.base_url.clone()))?;

        if response.status() == 404 {
            return Err(Error::MissingModel(self.model.clone()));
        }

        let mut note = String::new();
        for line in BufReader::new(response).lines().map_while(std::result::Result::ok) {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(chunk) = serde_json::from_str::<ChatChunk>(&line) else { continue };
            if let Some(message) = chunk.message {
                if !message.content.is_empty() {
                    on_token(&message.content);
                    note.push_str(&message.content);
                }
            }
            if chunk.done {
                break;
            }
        }
        Ok(note)
    }

    fn context_window(&self) -> usize {
        self.num_ctx
    }

    fn is_cloud(&self) -> bool {
        false
    }

    fn describe(&self) -> String {
        format!("{} · ollama", self.model)
    }
}

// ── the one thing callers need ─────────────────────────────────────────────

pub struct Note {
    pub body: String,
    pub model: String,
    pub prompt_version: i64,
}

/// Expands the fragments into a note. Returns `Ok(None)` when there is nothing
/// to expand — a meeting with no fragments has no signal about what mattered,
/// and a generic summary is precisely what this is not for.
pub fn enhance(
    provider: &dyn LlmProvider,
    turns: &[Turn],
    fragments: &[NoteFragment],
    on_token: &mut dyn FnMut(&str),
) -> Result<Option<Note>> {
    if fragments.is_empty() || turns.is_empty() {
        return Ok(None);
    }

    let user = prompt::build(turns, fragments);
    // Rough but adequate: the point is to warn before the tail is silently
    // dropped, not to count exactly.
    let estimated = (prompt::SYSTEM.len() + user.len()) / 4;
    if estimated > provider.context_window() {
        return Err(Error::Other(format!(
            "transcript is about {estimated} tokens but the model only reads {}. \
             The end of the meeting would be dropped without warning.",
            provider.context_window()
        )));
    }

    let body = provider.generate(prompt::SYSTEM, &user, on_token)?;
    Ok(Some(Note {
        body: body.trim().to_string(),
        model: provider.describe(),
        prompt_version: prompt::VERSION,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        window: usize,
    }
    impl LlmProvider for Fake {
        fn generate(&self, _: &str, _: &str, on_token: &mut dyn FnMut(&str)) -> Result<String> {
            on_token("## note\n");
            Ok("## note\nbody".into())
        }
        fn context_window(&self) -> usize {
            self.window
        }
        fn is_cloud(&self) -> bool {
            false
        }
        fn describe(&self) -> String {
            "fake".into()
        }
    }

    fn turn(text: &str) -> Turn {
        Turn { start_secs: 0.0, end_secs: 1.0, speaker: "ME".into(), speaker_name: None, text: text.into() }
    }

    #[test]
    fn no_fragments_means_no_note_rather_than_a_summary() {
        let note = enhance(&Fake { window: 32768 }, &[turn("hello")], &[], &mut |_| {}).unwrap();
        assert!(note.is_none(), "without fragments there is no signal about what mattered");
    }

    #[test]
    fn a_transcript_that_would_overflow_is_an_error_not_a_truncation() {
        let long = turn(&"word ".repeat(50_000));
        let fragments = vec![NoteFragment { at_secs: 1.0, text: "x".into() }];
        let outcome = enhance(&Fake { window: 4096 }, &[long], &fragments, &mut |_| {});
        // Silently dropping the tail is the failure this guards against: it
        // produces a confident note about the first part of the meeting.
        assert!(matches!(outcome, Err(Error::Other(_))));
    }

    #[test]
    fn tokens_stream_as_the_note_is_written() {
        let mut seen = String::new();
        let fragments = vec![NoteFragment { at_secs: 1.0, text: "x".into() }];
        let note = enhance(&Fake { window: 32768 }, &[turn("hi")], &fragments, &mut |t| seen.push_str(t))
            .unwrap()
            .unwrap();
        assert!(seen.starts_with("## note"));
        assert_eq!(note.prompt_version, prompt::VERSION);
    }
}
