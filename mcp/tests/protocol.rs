//! Drives the server the way Claude Code does: a real MCP handshake over stdio.
//!
//! Checks the contract rather than the internals — that the tools exist, that
//! they answer, and that nothing but protocol reaches stdout, which would
//! corrupt the stream for any real client.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use meetnote_core::{NoteFragment, Status, Store, Turn};

fn seeded_database(name: &str) -> (std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("meetnote-mcp-{}-{}", name, std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("meetnote.db");
    let _ = std::fs::remove_file(&path);

    let mut store = Store::open(&path).unwrap();
    let id = store.begin_meeting(1_755_000_000, "/tmp/rec").unwrap();
    store.finish_recording(&id, 1_755_002_040).unwrap();
    store
        .add_fragment(&id, &NoteFragment { at_secs: 134.0, text: "postgres migration 40%".into() })
        .unwrap();
    store
        .put_turns(&id, &[
            Turn { start_secs: 128.0, end_secs: 140.0, speaker: "ME".into(), speaker_name: None,
                   text: "The Postgres migration is about forty percent done.".into() },
            Turn { start_secs: 141.0, end_secs: 150.0, speaker: "THEM-1".into(), speaker_name: None,
                   text: "How many duplicates?".into() },
            Turn { start_secs: 151.0, end_secs: 158.0, speaker: "THEM-2".into(), speaker_name: None,
                   text: "Ship it before Friday.".into() },
        ])
        .unwrap();
    store
        .put_note(&id, "## postgres migration 40%\nForty percent complete.", "gpt-oss:20b", 1, 1_755_002_100)
        .unwrap();
    store.set_status(&id, Status::Done).unwrap();
    (path, id)
}

struct Client {
    child: std::process::Child,
    reader: BufReader<std::process::ChildStdout>,
    next_id: i64,
}

impl Client {
    fn start(database: &std::path::Path) -> Client {
        let mut child = Command::new(env!("CARGO_BIN_EXE_meetnote-mcp"))
            .env("MEETNOTE_DB", database)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("server did not start");
        let reader = BufReader::new(child.stdout.take().unwrap());
        Client { child, reader, next_id: 0 }
    }

    fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.next_id += 1;
        let request = serde_json::json!({
            "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params
        });
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{request}").unwrap();
        stdin.flush().unwrap();

        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line).unwrap() == 0 {
                panic!("server closed the stream during {method}");
            }
            // Every line on stdout must be protocol. A stray log here is a bug:
            // it would break any real client, not just this test.
            let message: serde_json::Value = serde_json::from_str(line.trim())
                .unwrap_or_else(|_| panic!("non-protocol output on stdout: {line:?}"));
            if message["id"] == self.next_id {
                return message;
            }
        }
    }

    fn notify(&mut self, method: &str) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{}", serde_json::json!({"jsonrpc": "2.0", "method": method})).unwrap();
        stdin.flush().unwrap();
    }

    fn tool(&mut self, name: &str, arguments: serde_json::Value) -> String {
        let response = self.call("tools/call", serde_json::json!({"name": name, "arguments": arguments}));
        response["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[test]
fn claude_code_can_read_the_notes() {
    let (database, meeting_id) = seeded_database("read");
    let mut client = Client::start(&database);

    let init = client.call(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "test", "version": "1"}
        }),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "meetnote");
    // The instructions tell the model how far to trust each layer; losing them
    // silently would be worse than losing a tool.
    let instructions = init["result"]["instructions"].as_str().unwrap_or_default();
    assert!(instructions.contains("ME is reliable"), "instructions missing the attribution caveat");
    client.notify("notifications/initialized");

    let listed = client.call("tools/list", serde_json::json!({}));
    let mut names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["get_note", "get_transcript", "list_meetings", "search_notes"]);

    // The user's own fragments come back with the meeting — how you recognise
    // which meeting you meant.
    let meetings = client.tool("list_meetings", serde_json::json!({"limit": 5}));
    assert!(meetings.contains("postgres migration 40%"));

    let note = client.tool("get_note", serde_json::json!({"meeting_id": meeting_id}));
    assert!(note.contains("Forty percent complete"));

    let hits = client.tool("search_notes", serde_json::json!({"query": "postgres"}));
    assert!(hits.contains(&meeting_id));

    let transcript = client.tool(
        "get_transcript",
        serde_json::json!({"meeting_id": meeting_id, "from_minute": 2, "to_minute": 3}),
    );
    assert!(transcript.contains("THEM-1") && transcript.contains("THEM-2"),
            "speaker separation must survive the round trip");

    let _ = std::fs::remove_dir_all(database.parent().unwrap());
}

#[test]
fn a_window_outside_the_meeting_returns_nothing_rather_than_everything() {
    let (database, meeting_id) = seeded_database("window");
    let mut client = Client::start(&database);
    client.call("initialize", serde_json::json!({
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "test", "version": "1"}
    }));
    client.notify("notifications/initialized");

    let transcript = client.tool(
        "get_transcript",
        serde_json::json!({"meeting_id": meeting_id, "from_minute": 90, "to_minute": 100}),
    );
    assert!(transcript.contains("\"lines\":[]"), "got: {transcript}");

    let _ = std::fs::remove_dir_all(database.parent().unwrap());
}
