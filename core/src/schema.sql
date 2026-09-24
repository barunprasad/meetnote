-- meetnote storage schema. See ARCHITECTURE.md for the reasoning; the notes
-- below cover only what is non-obvious from the column names.

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS meeting (
    id          TEXT PRIMARY KEY,
    started_at  INTEGER NOT NULL,
    ended_at    INTEGER,
    title       TEXT,
    source      TEXT,                        -- zoom|meet|teams|phone|unknown
    audio_dir   TEXT NOT NULL,
    -- recording|transcribing|enhancing|done|failed. Written at record start so
    -- a crash leaves a recoverable row rather than an orphan directory of wavs.
    status      TEXT NOT NULL,
    error       TEXT
);

CREATE TABLE IF NOT EXISTS stream (
    meeting_id    TEXT NOT NULL REFERENCES meeting(id) ON DELETE CASCADE,
    label         TEXT NOT NULL,             -- mic|system
    path          TEXT NOT NULL,
    sample_rate   INTEGER NOT NULL,
    channels      INTEGER NOT NULL,
    -- Host-clock offset captured at first buffer. The two-stream merge is built
    -- entirely on this; without it the streams cannot be put on one timeline.
    offset_secs   REAL NOT NULL,
    duration_secs REAL,
    -- A denied tap returns a full-length file of digital silence with no error,
    -- so "did we actually capture anything" has to be a stored fact.
    peak_dbfs     REAL,
    PRIMARY KEY (meeting_id, label)
);

CREATE TABLE IF NOT EXISTS turn (
    meeting_id   TEXT NOT NULL REFERENCES meeting(id) ON DELETE CASCADE,
    idx          INTEGER NOT NULL,
    start_secs   REAL NOT NULL,
    end_secs     REAL NOT NULL,
    speaker      TEXT NOT NULL,              -- ME|THEM|THEM-2…
    speaker_name TEXT,                       -- resolved later, from calendar
    text         TEXT NOT NULL,
    PRIMARY KEY (meeting_id, idx)
);

-- The user's live fragments, each stamped with when it was typed. That stamp is
-- what lets the enhancement prompt anchor a fragment to the transcript around
-- that moment instead of searching the whole meeting for what it referred to.
CREATE TABLE IF NOT EXISTS note_fragment (
    meeting_id  TEXT NOT NULL REFERENCES meeting(id) ON DELETE CASCADE,
    idx         INTEGER NOT NULL,
    at_secs     REAL NOT NULL,
    text        TEXT NOT NULL,
    PRIMARY KEY (meeting_id, idx)
);

CREATE TABLE IF NOT EXISTS enhanced_note (
    meeting_id     TEXT PRIMARY KEY REFERENCES meeting(id) ON DELETE CASCADE,
    body           TEXT NOT NULL,
    model          TEXT NOT NULL,
    -- The prompt is the product and it will change; notes written under an
    -- older revision have to stay identifiable.
    prompt_version INTEGER NOT NULL,
    created_at     INTEGER NOT NULL
);

-- Credentialed providers only. Ollama is detected at runtime, not configured,
-- so it never has a row. API keys live in the macOS Keychain, never here.
CREATE TABLE IF NOT EXISTS llm_provider (
    name        TEXT PRIMARY KEY,
    models      TEXT NOT NULL,               -- json array of model ids
    label       TEXT,
    verified_at INTEGER
);

-- A plain (non-external-content) FTS table: the corpus is small and rewriting a
-- row on note save is simpler than keeping sync triggers correct.
CREATE VIRTUAL TABLE IF NOT EXISTS note_fts USING fts5(
    meeting_id UNINDEXED,
    title,
    body
);

CREATE INDEX IF NOT EXISTS meeting_started_at ON meeting(started_at DESC);
CREATE INDEX IF NOT EXISTS meeting_status     ON meeting(status);
