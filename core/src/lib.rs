//! Storage and domain types shared by the meetnote app and its MCP server.
//!
//! The app opens this read-write; `meetnote-mcp` opens it read-only, so a
//! question asked from Claude Code can never corrupt a recording in progress.
//! WAL mode is what lets those two coexist without the reader blocking writes.

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("unknown meeting {0}")]
    UnknownMeeting(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Where a meeting is in the pipeline. Persisted at every transition so a crash
/// is recoverable rather than leaving an orphaned directory of wav files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Recording,
    Transcribing,
    Enhancing,
    Done,
    Failed,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Recording => "recording",
            Status::Transcribing => "transcribing",
            Status::Enhancing => "enhancing",
            Status::Done => "done",
            Status::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Status {
        match value {
            "transcribing" => Status::Transcribing,
            "enhancing" => Status::Enhancing,
            "done" => Status::Done,
            "failed" => Status::Failed,
            _ => Status::Recording,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meeting {
    pub id: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub title: Option<String>,
    pub source: Option<String>,
    pub audio_dir: String,
    pub status: Status,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stream {
    pub label: String,
    pub path: String,
    pub sample_rate: i64,
    pub channels: i64,
    pub offset_secs: f64,
    pub duration_secs: Option<f64>,
    pub peak_dbfs: Option<f64>,
}

impl Stream {
    /// A denied tap yields a full-length file of zeros and no error, so silence
    /// is a fact worth asking about directly rather than inferring later.
    /// Silent means measured and empty. An unknown level is NOT silence:
    /// treating it as such once made every healthy stream look denied, and
    /// transcription was skipped for every meeting with no error anywhere.
    pub fn is_silent(&self) -> bool {
        self.peak_dbfs.is_some_and(|db| db < -80.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub start_secs: f64,
    pub end_secs: f64,
    pub speaker: String,
    pub speaker_name: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteFragment {
    /// Seconds into the meeting when this was typed — the anchor the
    /// enhancement prompt uses to find the relevant stretch of transcript.
    pub at_secs: f64,
    pub text: String,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Store> {
        let conn = Connection::open(path)?;
        Store::configure(&conn)?;
        conn.execute_batch(include_str!("schema.sql"))?;
        Ok(Store { conn })
    }

    /// Read-only handle, for the MCP server. A question asked from Claude Code
    /// must not be able to touch a meeting that is still recording, and
    /// enforcing that at the connection rather than by convention means it
    /// cannot be got wrong later.
    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Store> {
        use rusqlite::OpenFlags;
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.pragma_update(None, "query_only", true)?;
        Ok(Store { conn })
    }

    /// In-memory store, for tests.
    pub fn open_memory() -> Result<Store> {
        let conn = Connection::open_in_memory()?;
        Store::configure(&conn)?;
        conn.execute_batch(include_str!("schema.sql"))?;
        Ok(Store { conn })
    }

    fn configure(conn: &Connection) -> Result<()> {
        // WAL so the MCP reader never blocks a write mid-meeting; foreign keys
        // so deleting a meeting actually takes its streams and turns with it.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(())
    }

    // ── recording ──────────────────────────────────────────────────────────

    /// Writes the meeting row before capture starts, so an interrupted session
    /// leaves something `recover_interrupted` can find.
    pub fn begin_meeting(&self, started_at: i64, audio_dir: &str) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO meeting (id, started_at, audio_dir, status)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, started_at, audio_dir, Status::Recording.as_str()],
        )?;
        Ok(id)
    }

    /// Names a meeting. Without this every meeting reads "Untitled meeting"
    /// everywhere, including in search results asked for from Claude Code,
    /// where being unable to tell two meetings apart makes the whole payoff
    /// much less useful.
    pub fn set_title(&self, meeting_id: &str, title: &str, source: Option<&str>) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE meeting SET title = ?2, source = COALESCE(?3, source) WHERE id = ?1",
            params![meeting_id, title, source],
        )?;
        if changed == 0 {
            return Err(Error::UnknownMeeting(meeting_id.into()));
        }
        // The note index carries the title, so a rename has to reach it too or
        // search keeps matching the old one.
        self.conn.execute(
            "UPDATE note_fts SET title = ?2 WHERE meeting_id = ?1",
            params![meeting_id, title],
        )?;
        Ok(())
    }

    /// Puts real names on diarised speakers — `THEM-1` becomes "Priya" once the
    /// calendar says who was invited.
    pub fn name_speaker(&self, meeting_id: &str, speaker: &str, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE turn SET speaker_name = ?3 WHERE meeting_id = ?1 AND speaker = ?2",
            params![meeting_id, speaker, name],
        )?;
        Ok(())
    }

    pub fn set_status(&self, meeting_id: &str, status: Status) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE meeting SET status = ?2 WHERE id = ?1",
            params![meeting_id, status.as_str()],
        )?;
        if changed == 0 {
            return Err(Error::UnknownMeeting(meeting_id.into()));
        }
        Ok(())
    }

    pub fn fail(&self, meeting_id: &str, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE meeting SET status = 'failed', error = ?2 WHERE id = ?1",
            params![meeting_id, error],
        )?;
        Ok(())
    }

    pub fn finish_recording(&self, meeting_id: &str, ended_at: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE meeting SET ended_at = ?2, status = 'transcribing' WHERE id = ?1",
            params![meeting_id, ended_at],
        )?;
        Ok(())
    }

    pub fn put_stream(&self, meeting_id: &str, stream: &Stream) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO stream
               (meeting_id, label, path, sample_rate, channels,
                offset_secs, duration_secs, peak_dbfs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                meeting_id,
                stream.label,
                stream.path,
                stream.sample_rate,
                stream.channels,
                stream.offset_secs,
                stream.duration_secs,
                stream.peak_dbfs,
            ],
        )?;
        Ok(())
    }

    /// Meetings still marked `recording` at launch — the app crashed or was
    /// killed mid-meeting. Their audio is on disk and can still be transcribed.
    pub fn recover_interrupted(&self) -> Result<Vec<Meeting>> {
        self.query_meetings("WHERE status = 'recording'", [])
    }

    // ── notes typed during the meeting ─────────────────────────────────────

    pub fn add_fragment(&self, meeting_id: &str, fragment: &NoteFragment) -> Result<()> {
        self.conn.execute(
            "INSERT INTO note_fragment (meeting_id, idx, at_secs, text)
             VALUES (?1,
                     (SELECT COALESCE(MAX(idx) + 1, 0) FROM note_fragment WHERE meeting_id = ?1),
                     ?2, ?3)",
            params![meeting_id, fragment.at_secs, fragment.text],
        )?;
        Ok(())
    }

    pub fn fragments(&self, meeting_id: &str) -> Result<Vec<NoteFragment>> {
        let mut stmt = self.conn.prepare(
            "SELECT at_secs, text FROM note_fragment WHERE meeting_id = ?1 ORDER BY idx",
        )?;
        let rows = stmt.query_map([meeting_id], |row| {
            Ok(NoteFragment { at_secs: row.get(0)?, text: row.get(1)? })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The words you have typed in earlier meetings, newest first.
    ///
    /// Whisper mangles the terms a team actually uses — "webhooks" came back as
    /// "wed hooks" — and its initial prompt fixes exactly that. Hints drawn
    /// from one meeting only help if you happened to type the term during it,
    /// so the vocabulary is carried across meetings: the tool learns your
    /// terminology by being used, with nothing to configure.
    pub fn recent_vocabulary(&self, excluding: &str, limit: usize) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.text
               FROM note_fragment f
               JOIN meeting m ON m.id = f.meeting_id
              WHERE f.meeting_id <> ?1
              ORDER BY m.started_at DESC, f.idx DESC
              LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![excluding, limit as i64], |row| row.get::<_, String>(0))?;
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for text in rows {
            let text = text?;
            if seen.insert(text.to_lowercase()) {
                out.push(text);
            }
        }
        Ok(out)
    }

    /// Removes a meeting and everything hanging off it, returning the audio
    /// folder so the caller can delete the files. The store owns rows, not
    /// files: a half-deleted meeting whose audio lingers is worse than either.
    pub fn delete_meeting(&mut self, meeting_id: &str) -> Result<String> {
        let meeting = self
            .meeting(meeting_id)?
            .ok_or_else(|| Error::UnknownMeeting(meeting_id.into()))?;
        let tx = self.conn.transaction()?;
        // The children cascade, but note_fts is a plain table with no foreign
        // key, so a deleted meeting would otherwise keep turning up in search.
        tx.execute("DELETE FROM note_fts WHERE meeting_id = ?1", [meeting_id])?;
        tx.execute("DELETE FROM meeting WHERE id = ?1", [meeting_id])?;
        tx.commit()?;
        Ok(meeting.audio_dir)
    }

    /// Forgets the recordings and keeps the meeting. Audio is ~350 MB an hour
    /// while the transcript and note are kilobytes, so this reclaims nearly
    /// everything without losing what the meeting was for. The empty
    /// `audio_dir` is how the rest of the app knows the audio is gone.
    pub fn forget_audio(&mut self, meeting_id: &str) -> Result<String> {
        let meeting = self
            .meeting(meeting_id)?
            .ok_or_else(|| Error::UnknownMeeting(meeting_id.into()))?;
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM stream WHERE meeting_id = ?1", [meeting_id])?;
        tx.execute("UPDATE meeting SET audio_dir = '' WHERE id = ?1", [meeting_id])?;
        tx.commit()?;
        Ok(meeting.audio_dir)
    }

    // ── transcript ─────────────────────────────────────────────────────────

    /// Replaces the transcript wholesale — re-transcribing with a different
    /// engine should not merge with the previous engine's output.
    pub fn put_turns(&mut self, meeting_id: &str, turns: &[Turn]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM turn WHERE meeting_id = ?1", [meeting_id])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO turn
                   (meeting_id, idx, start_secs, end_secs, speaker, speaker_name, text)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (idx, turn) in turns.iter().enumerate() {
                stmt.execute(params![
                    meeting_id,
                    idx as i64,
                    turn.start_secs,
                    turn.end_secs,
                    turn.speaker,
                    turn.speaker_name,
                    turn.text,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Windowed on purpose: an hour of dialogue is 12–18k tokens and returning
    /// it whole would blow out an MCP caller's context window.
    pub fn turns(&self, meeting_id: &str, from: f64, to: f64) -> Result<Vec<Turn>> {
        let mut stmt = self.conn.prepare(
            "SELECT start_secs, end_secs, speaker, speaker_name, text
             FROM turn
             WHERE meeting_id = ?1 AND end_secs >= ?2 AND start_secs <= ?3
             ORDER BY idx",
        )?;
        let rows = stmt.query_map(params![meeting_id, from, to], |row| {
            Ok(Turn {
                start_secs: row.get(0)?,
                end_secs: row.get(1)?,
                speaker: row.get(2)?,
                speaker_name: row.get(3)?,
                text: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ── the note ───────────────────────────────────────────────────────────

    pub fn put_note(
        &mut self,
        meeting_id: &str,
        body: &str,
        model: &str,
        prompt_version: i64,
        created_at: i64,
    ) -> Result<()> {
        let title: Option<String> = self
            .conn
            .query_row("SELECT title FROM meeting WHERE id = ?1", [meeting_id], |r| r.get(0))
            .optional()?
            .flatten();

        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO enhanced_note
               (meeting_id, body, model, prompt_version, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![meeting_id, body, model, prompt_version, created_at],
        )?;
        // Rewrite rather than sync-trigger: the corpus is small and correctness
        // here is easier to see than to maintain.
        tx.execute("DELETE FROM note_fts WHERE meeting_id = ?1", [meeting_id])?;
        tx.execute(
            "INSERT INTO note_fts (meeting_id, title, body) VALUES (?1, ?2, ?3)",
            params![meeting_id, title.unwrap_or_default(), body],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn note(&self, meeting_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT body FROM enhanced_note WHERE meeting_id = ?1",
                [meeting_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT meeting_id, snippet(note_fts, 2, '', '', '…', 24)
             FROM note_fts WHERE note_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![query, limit], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ── reads ──────────────────────────────────────────────────────────────

    pub fn meeting(&self, meeting_id: &str) -> Result<Option<Meeting>> {
        Ok(self.query_meetings("WHERE id = ?1", [meeting_id])?.into_iter().next())
    }

    pub fn recent(&self, limit: i64) -> Result<Vec<Meeting>> {
        self.query_meetings("ORDER BY started_at DESC LIMIT ?1", [limit])
    }

    pub fn streams(&self, meeting_id: &str) -> Result<Vec<Stream>> {
        let mut stmt = self.conn.prepare(
            "SELECT label, path, sample_rate, channels, offset_secs, duration_secs, peak_dbfs
             FROM stream WHERE meeting_id = ?1 ORDER BY label",
        )?;
        let rows = stmt.query_map([meeting_id], |row| {
            Ok(Stream {
                label: row.get(0)?,
                path: row.get(1)?,
                sample_rate: row.get(2)?,
                channels: row.get(3)?,
                offset_secs: row.get(4)?,
                duration_secs: row.get(5)?,
                peak_dbfs: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    fn query_meetings<P: rusqlite::Params>(&self, clause: &str, params: P) -> Result<Vec<Meeting>> {
        let sql = format!(
            "SELECT id, started_at, ended_at, title, source, audio_dir, status, error
             FROM meeting {clause}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params, |row| {
            Ok(Meeting {
                id: row.get(0)?,
                started_at: row.get(1)?,
                ended_at: row.get(2)?,
                title: row.get(3)?,
                source: row.get(4)?,
                audio_dir: row.get(5)?,
                status: Status::parse(&row.get::<_, String>(6)?),
                error: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meeting(store: &Store) -> String {
        store.begin_meeting(1_700_000_000, "/tmp/rec").unwrap()
    }

    #[test]
    fn a_crash_mid_meeting_leaves_a_recoverable_row() {
        let store = Store::open_memory().unwrap();
        let id = meeting(&store);
        // No finish_recording — simulates the process dying mid-capture.
        let stuck = store.recover_interrupted().unwrap();
        assert_eq!(stuck.len(), 1);
        assert_eq!(stuck[0].id, id);

        store.finish_recording(&id, 1_700_000_600).unwrap();
        assert!(store.recover_interrupted().unwrap().is_empty());
        assert_eq!(store.meeting(&id).unwrap().unwrap().status, Status::Transcribing);
    }

    #[test]
    fn a_silent_stream_is_detectable_from_stored_state() {
        let store = Store::open_memory().unwrap();
        let id = meeting(&store);
        store
            .put_stream(&id, &Stream {
                label: "system".into(), path: "system.wav".into(),
                sample_rate: 48000, channels: 2, offset_secs: 0.003,
                duration_secs: Some(30.0), peak_dbfs: Some(-96.0),
            })
            .unwrap();
        store
            .put_stream(&id, &Stream {
                label: "mic".into(), path: "mic.wav".into(),
                sample_rate: 48000, channels: 1, offset_secs: 0.0,
                duration_secs: Some(30.0), peak_dbfs: Some(-24.6),
            })
            .unwrap();

        let streams = store.streams(&id).unwrap();
        let system = streams.iter().find(|s| s.label == "system").unwrap();
        let mic = streams.iter().find(|s| s.label == "mic").unwrap();
        assert!(system.is_silent(), "a denied tap must be visible in the data");
        assert!(!mic.is_silent());
        // The offset is what the merge depends on — it must survive the round trip.
        assert!((system.offset_secs - 0.003).abs() < f64::EPSILON);
    }

    #[test]
    fn retranscribing_replaces_rather_than_merges() {
        let mut store = Store::open_memory().unwrap();
        let id = meeting(&store);
        let turn = |t: &str| Turn {
            start_secs: 0.0, end_secs: 1.0, speaker: "ME".into(),
            speaker_name: None, text: t.into(),
        };
        store.put_turns(&id, &[turn("first engine"), turn("second line")]).unwrap();
        store.put_turns(&id, &[turn("other engine")]).unwrap();

        let turns = store.turns(&id, 0.0, 1000.0).unwrap();
        assert_eq!(turns.len(), 1, "old engine's turns must not linger");
        assert_eq!(turns[0].text, "other engine");
    }

    #[test]
    fn transcript_reads_are_windowed() {
        let mut store = Store::open_memory().unwrap();
        let id = meeting(&store);
        let turns: Vec<Turn> = (0..100)
            .map(|i| Turn {
                start_secs: i as f64 * 10.0,
                end_secs: i as f64 * 10.0 + 9.0,
                speaker: "THEM".into(),
                speaker_name: None,
                text: format!("turn {i}"),
            })
            .collect();
        store.put_turns(&id, &turns).unwrap();

        let window = store.turns(&id, 100.0, 200.0).unwrap();
        assert!(window.len() < 20, "a window must not return the whole hour");
        assert!(window.iter().all(|t| t.end_secs >= 100.0 && t.start_secs <= 200.0));
    }

    #[test]
    fn vocabulary_carries_across_meetings_without_repeating_itself() {
        let store = Store::open_memory().unwrap();
        let older = meeting(&store);
        let current = meeting(&store);
        for (id, text) in [
            (&older, "webhook dupes"),
            (&older, "Webhook Dupes"),
            (&older, "postgres migration"),
            (&current, "todays note"),
        ] {
            store
                .add_fragment(id, &NoteFragment { at_secs: 1.0, text: text.into() })
                .unwrap();
        }

        let vocab = store.recent_vocabulary(&current, 20).unwrap();
        let has = |term: &str| vocab.iter().any(|v| v.to_lowercase() == term);
        assert!(has("webhook dupes"), "got {vocab:?}");
        assert!(has("postgres migration"), "got {vocab:?}");
        assert!(
            !vocab.iter().any(|v| v == "todays note"),
            "the meeting being transcribed supplies its own hints"
        );
        assert_eq!(
            vocab.iter().filter(|v| v.to_lowercase() == "webhook dupes").count(),
            1,
            "a term typed twice is one hint, not two"
        );
    }

    #[test]
    fn fragments_keep_their_timestamps_and_order() {
        let store = Store::open_memory().unwrap();
        let id = meeting(&store);
        for (at, text) in [(12.5, "postgres migration 40%"), (94.0, "friday ship?")] {
            store.add_fragment(&id, &NoteFragment { at_secs: at, text: text.into() }).unwrap();
        }
        let fragments = store.fragments(&id).unwrap();
        assert_eq!(fragments.len(), 2);
        assert_eq!(fragments[0].text, "postgres migration 40%");
        assert!((fragments[1].at_secs - 94.0).abs() < f64::EPSILON);
    }

    #[test]
    fn notes_are_searchable_and_re_enhancing_does_not_duplicate() {
        let mut store = Store::open_memory().unwrap();
        let id = meeting(&store);
        store.put_note(&id, "Postgres migration is 40% done", "gpt-oss:20b", 1, 100).unwrap();
        assert_eq!(store.search("postgres", 10).unwrap().len(), 1);

        store.put_note(&id, "Rewritten under a newer prompt", "gpt-oss:20b", 2, 200).unwrap();
        assert_eq!(store.search("postgres", 10).unwrap().len(), 0, "stale text must not linger");
        assert_eq!(store.search("rewritten", 10).unwrap().len(), 1);
        assert_eq!(store.note(&id).unwrap().unwrap(), "Rewritten under a newer prompt");
    }

    #[test]
    fn deleting_a_meeting_takes_its_children() {
        let mut store = Store::open_memory().unwrap();
        let id = meeting(&store);
        store.put_turns(&id, &[Turn {
            start_secs: 0.0, end_secs: 1.0, speaker: "ME".into(),
            speaker_name: None, text: "hello".into(),
        }]).unwrap();
        store.conn.execute("DELETE FROM meeting WHERE id = ?1", [&id]).unwrap();
        assert!(store.turns(&id, 0.0, 1000.0).unwrap().is_empty());
    }

    #[test]
    fn deleting_a_meeting_also_takes_it_out_of_search() {
        let mut store = Store::open_memory().unwrap();
        let id = meeting(&store);
        store.set_title(&id, "Payments incident review", Some("manual")).unwrap();
        store.put_note(&id, "the provider retried on timeouts", "m", 1, 1).unwrap();
        assert!(!store.search("timeouts", 10).unwrap().is_empty());

        let audio_dir = store.delete_meeting(&id).unwrap();
        assert_eq!(audio_dir, "/tmp/rec", "the caller needs the folder to delete it");
        assert!(store.meeting(&id).unwrap().is_none());
        assert!(
            store.search("timeouts", 10).unwrap().is_empty(),
            "a deleted meeting must not survive in the search index"
        );
        assert!(matches!(
            store.delete_meeting(&id),
            Err(Error::UnknownMeeting(_))
        ));
    }

    #[test]
    fn forgetting_audio_keeps_the_note_and_the_transcript() {
        let mut store = Store::open_memory().unwrap();
        let id = meeting(&store);
        store.put_turns(&id, &[Turn {
            start_secs: 0.0, end_secs: 1.0, speaker: "ME".into(),
            speaker_name: None, text: "hello".into(),
        }]).unwrap();
        store.put_note(&id, "a note worth keeping", "m", 1, 1).unwrap();
        store.put_stream(&id, &Stream {
            label: "mic".into(), path: "mic.wav".into(), sample_rate: 48000, channels: 1,
            offset_secs: 0.0, duration_secs: Some(30.0), peak_dbfs: Some(-20.0),
        }).unwrap();

        let audio_dir = store.forget_audio(&id).unwrap();
        assert_eq!(audio_dir, "/tmp/rec");
        let kept = store.meeting(&id).unwrap().unwrap();
        assert_eq!(kept.audio_dir, "", "an empty folder is how the app knows the audio is gone");
        assert!(store.streams(&id).unwrap().is_empty());
        assert_eq!(store.turns(&id, 0.0, 1000.0).unwrap().len(), 1);
        assert_eq!(store.note(&id).unwrap().as_deref(), Some("a note worth keeping"));
    }

    #[test]
    fn unknown_meeting_is_an_error_not_a_silent_noop() {
        let store = Store::open_memory().unwrap();
        assert!(matches!(
            store.set_status("nope", Status::Done),
            Err(Error::UnknownMeeting(_))
        ));
    }
}

#[cfg(test)]
mod readonly_tests {
    use super::*;

    #[test]
    fn a_readonly_store_cannot_write() {
        let dir = std::env::temp_dir().join(format!("meetnote-ro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        let _ = std::fs::remove_file(&path);

        let writer = Store::open(&path).unwrap();
        let id = writer.begin_meeting(1_700_000_000, "/tmp/rec").unwrap();
        writer.finish_recording(&id, 1_700_000_600).unwrap();
        drop(writer);

        let reader = Store::open_readonly(&path).unwrap();
        // Reads work…
        assert_eq!(reader.recent(10).unwrap().len(), 1);
        // …writes are refused by the connection itself, not by our own care.
        assert!(reader.set_status(&id, Status::Failed).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod naming_tests {
    use super::*;

    #[test]
    fn renaming_a_meeting_updates_what_search_matches() {
        let mut store = Store::open_memory().unwrap();
        let id = store.begin_meeting(1_700_000_000, "/tmp/rec").unwrap();
        store.put_note(&id, "the body", "m", 1, 1).unwrap();
        store.set_title(&id, "Platform sync", None).unwrap();

        assert_eq!(store.meeting(&id).unwrap().unwrap().title.as_deref(), Some("Platform sync"));
        // Search has to follow the rename, or it keeps finding the old title.
        assert_eq!(store.search("Platform", 10).unwrap().len(), 1);
    }

    #[test]
    fn naming_a_speaker_only_touches_that_speaker() {
        let mut store = Store::open_memory().unwrap();
        let id = store.begin_meeting(1_700_000_000, "/tmp/rec").unwrap();
        let turn = |speaker: &str| Turn {
            start_secs: 0.0, end_secs: 1.0, speaker: speaker.into(),
            speaker_name: None, text: "hi".into(),
        };
        store.put_turns(&id, &[turn("THEM-1"), turn("THEM-2"), turn("ME")]).unwrap();
        store.name_speaker(&id, "THEM-1", "Priya").unwrap();

        let turns = store.turns(&id, 0.0, 100.0).unwrap();
        assert_eq!(turns[0].speaker_name.as_deref(), Some("Priya"));
        assert_eq!(turns[1].speaker_name, None);
        assert_eq!(turns[2].speaker_name, None, "ME is the user, never a calendar attendee");
    }
}
