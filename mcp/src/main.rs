//! meetnote-mcp — your meeting notes, readable from Claude Code.
//!
//! This is the payoff the brief was written for: notes stop being something you
//! open an app to read and become something you can ask about while working.
//!
//! Runs as its own process over stdio, opening the database **read-only**, so a
//! question asked here can never touch a meeting that is still recording. It
//! also works whether or not the app is running — the notes are just a file.

use std::path::PathBuf;
use std::sync::Mutex;

use meetnote_core::Store;
use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt,
};
use serde::{Deserialize, Serialize};

fn database_path() -> PathBuf {
    std::env::var("MEETNOTE_DB").map(PathBuf::from).unwrap_or_else(|_| {
        dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("meetnote").join("meetnote.db")
    })
}

fn day(epoch: i64) -> String {
    chrono::DateTime::from_timestamp(epoch, 0)
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

fn minutes(meeting: &meetnote_core::Meeting) -> i64 {
    meeting.ended_at.map(|end| (end - meeting.started_at) / 60).unwrap_or(0)
}

struct Meetnote {
    store: Mutex<Store>,
}

// ── tool inputs and outputs ────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct RecentInput {
    /// How many meetings to return. Defaults to 20.
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Serialize, schemars::JsonSchema)]
struct MeetingSummary {
    id: String,
    title: String,
    when: String,
    minutes: i64,
    status: String,
    /// The user's own live fragments — what they thought was worth writing
    /// down. More useful for recognising a meeting than a generated summary.
    notes: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct MeetingInput {
    /// Meeting id, as returned by list_meetings or search_notes.
    meeting_id: String,
}

#[derive(Serialize, schemars::JsonSchema)]
struct NoteOutput {
    title: String,
    when: String,
    /// Absent until the meeting has been transcribed and written up.
    note: Option<String>,
    status: String,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct SearchInput {
    /// Full-text query across meeting notes.
    query: String,
}

#[derive(Serialize, schemars::JsonSchema)]
struct SearchHit {
    meeting_id: String,
    title: String,
    when: String,
    excerpt: String,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct TranscriptInput {
    meeting_id: String,
    /// Start of the window, in minutes into the meeting. Defaults to 0.
    #[serde(default)]
    from_minute: Option<f64>,
    /// End of the window, in minutes. Defaults to ten minutes after the start.
    #[serde(default)]
    to_minute: Option<f64>,
}

#[derive(Serialize, schemars::JsonSchema)]
struct TranscriptOutput {
    /// `ME` is the person recording; `THEM-1`, `THEM-2` are the other
    /// participants, separated by which audio stream they arrived on.
    lines: Vec<String>,
    window: String,
    truncated: bool,
}

// ── tools ──────────────────────────────────────────────────────────────────

#[tool_router]
impl Meetnote {
    #[tool(
        name = "list_meetings",
        description = "List recent meetings, newest first, with the notes the user typed during each one."
    )]
    fn list_meetings(&self, Parameters(input): Parameters<RecentInput>) -> Json<Vec<MeetingSummary>> {
        let store = self.store.lock().unwrap();
        let limit = input.limit.unwrap_or(20).min(200) as i64;
        let summaries = store
            .recent(limit)
            .unwrap_or_default()
            .into_iter()
            .map(|meeting| MeetingSummary {
                notes: store
                    .fragments(&meeting.id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|f| f.text)
                    .collect(),
                title: meeting.title.clone().unwrap_or_else(|| "Untitled meeting".into()),
                when: day(meeting.started_at),
                minutes: minutes(&meeting),
                status: format!("{:?}", meeting.status).to_lowercase(),
                id: meeting.id,
            })
            .collect();
        Json(summaries)
    }

    #[tool(
        name = "get_note",
        description = "Read the written-up note for one meeting: the user's fragments expanded using the transcript."
    )]
    fn get_note(&self, Parameters(input): Parameters<MeetingInput>) -> Json<NoteOutput> {
        let store = self.store.lock().unwrap();
        let meeting = store.meeting(&input.meeting_id).ok().flatten();
        Json(NoteOutput {
            title: meeting
                .as_ref()
                .and_then(|m| m.title.clone())
                .unwrap_or_else(|| "Untitled meeting".into()),
            when: meeting.as_ref().map(|m| day(m.started_at)).unwrap_or_default(),
            status: meeting
                .as_ref()
                .map(|m| format!("{:?}", m.status).to_lowercase())
                .unwrap_or_else(|| "unknown".into()),
            note: store.note(&input.meeting_id).ok().flatten(),
        })
    }

    #[tool(
        name = "search_notes",
        description = "Search across all meeting notes by keyword and return matching meetings with an excerpt."
    )]
    fn search_notes(&self, Parameters(input): Parameters<SearchInput>) -> Json<Vec<SearchHit>> {
        let store = self.store.lock().unwrap();
        let hits = store
            .search(&input.query, 30)
            .unwrap_or_default()
            .into_iter()
            .map(|(meeting_id, excerpt)| {
                let meeting = store.meeting(&meeting_id).ok().flatten();
                SearchHit {
                    title: meeting
                        .as_ref()
                        .and_then(|m| m.title.clone())
                        .unwrap_or_else(|| "Untitled meeting".into()),
                    when: meeting.map(|m| day(m.started_at)).unwrap_or_default(),
                    meeting_id,
                    excerpt,
                }
            })
            .collect();
        Json(hits)
    }

    #[tool(
        name = "get_transcript",
        description = "Read what was actually said, for one window of a meeting. Windowed because an hour of dialogue is far too large to return whole."
    )]
    fn get_transcript(&self, Parameters(input): Parameters<TranscriptInput>) -> Json<TranscriptOutput> {
        let store = self.store.lock().unwrap();
        let from = input.from_minute.unwrap_or(0.0).max(0.0);
        let to = input.to_minute.unwrap_or(from + 10.0).max(from);

        let mut lines: Vec<String> = store
            .turns(&input.meeting_id, from * 60.0, to * 60.0)
            .unwrap_or_default()
            .into_iter()
            .map(|turn| {
                let at = turn.start_secs as i64;
                let who = turn.speaker_name.unwrap_or(turn.speaker);
                format!("[{:02}:{:02}] {}: {}", at / 60, at % 60, who, turn.text)
            })
            .collect();

        // A hard stop as well as a time window: a dense stretch of conversation
        // could still overflow the caller's context even inside ten minutes.
        let truncated = lines.len() > 400;
        lines.truncate(400);

        Json(TranscriptOutput {
            window: format!("{from:.0}–{to:.0} min"),
            lines,
            truncated,
        })
    }
}

/// Instructions matter as much as the tools: they are what tells the model how
/// these notes are built, and therefore how far to trust them.
#[tool_handler]
impl ServerHandler for Meetnote {
    fn get_info(&self) -> ServerInfo {
        let mut implementation = Implementation::default();
        implementation.name = "meetnote".into();
        implementation.version = env!("CARGO_PKG_VERSION").into();

        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::V_2025_06_18;
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = implementation;
        info.instructions = Some(
                "The user's own meeting notes, recorded and transcribed locally on this Mac.\n\n                 Each meeting has three layers, and which one to read depends on the question:\n                 - the fragments the user typed live, returned by list_meetings — terse, and the                  best signal of what they thought mattered;\n                 - the written-up note (get_note), which expands those fragments using the                  transcript as evidence;\n                 - what was actually said (get_transcript), windowed by minute.\n\n                 Speakers are labelled ME for the user and THEM-1, THEM-2 for other participants.                  ME is reliable — it comes from which microphone the audio arrived on, not from a                  model's guess. The THEM-n split does come from a model, so treat it as probable                  rather than certain.\n\n                 A note marked unverified was something the user wrote that the transcript does                  not support; quote it as their words, not as an established fact."
                    .into(),
        );
        info
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let path = database_path();
    let store = Store::open_readonly(&path).map_err(|error| {
        anyhow::anyhow!("cannot read {} ({error}). Record a meeting in meetnote first.", path.display())
    })?;

    // Everything goes over stdio, so anything written to stdout that is not
    // protocol would corrupt the stream. Diagnostics belong on stderr.
    eprintln!("meetnote-mcp reading {}", path.display());

    let service = Meetnote { store: Mutex::new(store) }
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}
