//! meetnote — recording orchestration and reads for the UI.
//!
//! Everything that touches audio, the database, or a model runs here rather
//! than in the WebView: the transcript is a recording of a private meeting, and
//! API keys for any cloud provider live in the Keychain. The frontend only ever
//! sees what these commands return.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use meetnote_core::{Meeting, NoteFragment, Participant, Status, Store, Stream, Turn};
use serde::{Deserialize, Serialize};
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder,
};

// ── state ──────────────────────────────────────────────────────────────────

struct Recording {
    meeting_id: String,
    child: Child,
    started_at: i64,
}

#[derive(Default)]
struct App {
    store: Mutex<Option<Store>>,
    recording: Mutex<Option<Recording>>,
    /// Held for the whole of a transcription. Whisper and the diarisation
    /// models each want most of the machine, so a second job (a recovered
    /// meeting at launch, a "transcribe again") waits its turn rather than
    /// running alongside.
    transcribing: Mutex<()>,
    call: Mutex<CallState>,
}

/// What the call watcher last reported, kept here so a panel opened later can
/// ask rather than wait for the next event.
#[derive(Default)]
struct CallState {
    current: Option<CallEvent>,
    /// "Not now" for this call: stay quiet until it ends.
    dismissed: bool,
    /// Set when the call ends mid-recording. The recording stops at this
    /// moment unless you choose to keep it going or the call comes back.
    stop_at: Option<std::time::Instant>,
}

/// How long a recording outlives its call. Long enough to rejoin after a
/// dropped connection; short enough that a forgotten recording doesn't keep
/// running into the next hour.
const STOP_AFTER_CALL: std::time::Duration = std::time::Duration::from_secs(60);

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("meetnote")
}

/// Sidecars sit beside the host binary: `Contents/MacOS` when bundled,
/// `target/debug` under `tauri dev`, where the target-triple suffix survives.
fn sidecar() -> Result<PathBuf, String> {
    let dir = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .ok_or("no parent directory")?
        .to_path_buf();
    for name in ["meetnote-capture", "meetnote-capture-aarch64-apple-darwin"] {
        let candidate = dir.join(name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(format!("capture sidecar not found beside {}", dir.display()))
}

fn with_store<T>(app: &App, f: impl FnOnce(&mut Store) -> Result<T, String>) -> Result<T, String> {
    let mut guard = app.store.lock().map_err(|e| e.to_string())?;
    let store = guard.as_mut().ok_or("storage not opened")?;
    f(store)
}

// ── events pushed to the UI ────────────────────────────────────────────────

/// A meeting starting or stopping, detected by a process using the microphone
/// and speakers at once. The app offers to record; it never starts on its own.
#[derive(Clone, Serialize)]
struct CallEvent {
    state: String,
    name: String,
    app: String,
}

#[derive(Clone, Serialize)]
struct Level {
    t: f64,
    mic_dbfs: f64,
    system_dbfs: f64,
}

#[derive(Deserialize)]
struct SidecarStream {
    label: String,
    path: String,
    sample_rate: i64,
    channels: i64,
    offset_seconds: f64,
    duration_seconds: f64,
    silent: bool,
    /// Absent from sidecars built before the level was reported.
    #[serde(default)]
    peak_dbfs: Option<f64>,
    /// Host-clock span of the stream — the independent check on the header's
    /// rate. Absent from older sidecars.
    #[serde(default)]
    wall_seconds: Option<f64>,
}

// ── recording ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct StartedRecording {
    meeting_id: String,
}

#[tauri::command]
fn start_recording(app: AppHandle, state: State<App>) -> Result<StartedRecording, String> {
    if state.recording.lock().map_err(|e| e.to_string())?.is_some() {
        return Err("already recording".into());
    }

    let started_at = now();
    let audio_dir = data_dir().join("recordings").join(started_at.to_string());
    std::fs::create_dir_all(&audio_dir).map_err(|e| e.to_string())?;

    // The row is written before capture starts, so a crash leaves something
    // `recover_interrupted` can find rather than an orphan directory of wavs.
    let meeting_id = with_store(&state, |store| {
        store
            .begin_meeting(started_at, &audio_dir.to_string_lossy())
            .map_err(|e| e.to_string())
    })?;

    let mut child = Command::new(sidecar()?)
        .args(["--json", "--out", &audio_dir.to_string_lossy()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start capture: {e}"))?;

    // Levels arrive as newline-delimited JSON and go straight to the meters.
    // A flat trace is the shape of a failed capture, so the UI needs them live
    // rather than at the end.
    if let Some(stdout) = child.stdout.take() {
        let app = app.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                if event.get("type").and_then(|t| t.as_str()) == Some("level") {
                    let _ = app.emit(
                        "capture:level",
                        Level {
                            t: event["t"].as_f64().unwrap_or(0.0),
                            mic_dbfs: event["mic_dbfs"].as_f64().unwrap_or(-120.0),
                            system_dbfs: event["system_dbfs"].as_f64().unwrap_or(-120.0),
                        },
                    );
                }
            }
        });
    }

    // Named at the start rather than the end: the calendar event is happening
    // now, and by the time the meeting stops it may already be over.
    if let Some(event) = calendar_event(None, true) {
        let _ = with_store(&state, |store| {
            store
                .set_title(&meeting_id, &event.title, Some("calendar"))
                .map_err(|e| e.to_string())?;
            store
                .set_participants(&meeting_id, &event.participants())
                .map_err(|e| e.to_string())
        });
    }

    *state.recording.lock().map_err(|e| e.to_string())? = Some(Recording {
        meeting_id: meeting_id.clone(),
        child,
        started_at,
    });
    show_panel(&app, PanelMode::Notes);
    refresh_tray(&app, true);
    let _ = app.emit("recording:started", &meeting_id);
    Ok(StartedRecording { meeting_id })
}

#[derive(Serialize)]
struct Captured {
    meeting_id: String,
    streams: Vec<StoredStream>,
}

#[derive(Serialize)]
struct StoredStream {
    label: String,
    duration_secs: f64,
    peak_dbfs: Option<f64>,
    silent: bool,
}

#[tauri::command]
fn stop_recording(app: AppHandle, state: State<App>) -> Result<Captured, String> {
    hide_panel(&app);
    refresh_tray(&app, false);
    if let Ok(mut call) = state.call.lock() {
        call.stop_at = None;
    }
    let _ = app.emit("recording:stopped", ());
    let mut recording = state
        .recording
        .lock()
        .map_err(|e| e.to_string())?
        .take()
        .ok_or("not recording")?;

    // SIGINT rather than kill: the sidecar closes its audio files on interrupt,
    // and a hard kill would leave the last seconds unwritten.
    #[allow(unsafe_code)]
    unsafe {
        libc::kill(recording.child.id() as i32, libc::SIGINT);
    }
    let _ = recording.child.wait();

    let meeting_id = recording.meeting_id.clone();
    let audio_dir = data_dir()
        .join("recordings")
        .join(recording.started_at.to_string());
    let stored = with_store(&state, |store| ingest_capture(store, &meeting_id, &audio_dir, now()))?;

    transcribe_in_background(app, meeting_id.clone(), false);
    Ok(Captured { meeting_id, streams: stored })
}

/// Records what the sidecar captured and marks the meeting ready to transcribe.
/// Shared by a normal stop and by recovery after a crash.
fn ingest_capture(
    store: &mut Store,
    meeting_id: &str,
    audio_dir: &std::path::Path,
    ended_at: i64,
) -> Result<Vec<StoredStream>, String> {
    let meta = std::fs::read_to_string(audio_dir.join("capture.json"))
        .map_err(|e| format!("capture produced no metadata: {e}"))?;
    let parsed: serde_json::Value = serde_json::from_str(&meta).map_err(|e| e.to_string())?;
    let streams: Vec<SidecarStream> =
        serde_json::from_value(parsed["streams"].clone()).map_err(|e| e.to_string())?;

    let mut stored = Vec::new();
    for s in &streams {
        // The sidecar measures the loudest sample; keep it as a level so the
        // capture check can show what was heard rather than a boolean. Older
        // sidecars reported only `silent`, so fall back to the floor for
        // those — but never leave a healthy stream without a level, which
        // used to make every stream look silent and skip transcription.
        let peak = s.peak_dbfs.or(if s.silent { Some(-120.0) } else { None });
        let mut stream = Stream {
            label: s.label.clone(),
            path: s.path.clone(),
            sample_rate: s.sample_rate,
            channels: s.channels,
            offset_secs: s.offset_seconds,
            duration_secs: Some(s.duration_seconds),
            peak_dbfs: peak,
        };
        if let Some(wall) = s.wall_seconds {
            correct_rate(&mut stream, wall);
        }
        store.put_stream(meeting_id, &stream).map_err(|e| e.to_string())?;
        stored.push(StoredStream {
            label: stream.label,
            duration_secs: stream.duration_secs.unwrap_or_default(),
            peak_dbfs: peak,
            silent: s.silent,
        });
    }
    store.finish_recording(meeting_id, ended_at).map_err(|e| e.to_string())?;
    Ok(stored)
}

/// Fixes a stream whose header claims the wrong rate, judged against how long
/// it really ran. Returns whether anything changed. The wav itself is left as
/// recorded; the stored rate is what transcription reads it at.
fn correct_rate(stream: &mut Stream, reference_secs: f64) -> bool {
    let Some(rate) = stream.true_rate(reference_secs) else { return false };
    eprintln!(
        "{} stream: header says {} Hz but it ran {:.0}s — reading it at {} Hz",
        stream.label, stream.sample_rate, reference_secs, rate
    );
    let frames = stream.duration_secs.unwrap_or_default() * stream.sample_rate as f64;
    stream.sample_rate = rate;
    stream.duration_secs = Some(frames / rate as f64);
    true
}

#[tauri::command]
fn is_recording(state: State<App>) -> Result<bool, String> {
    Ok(state.recording.lock().map_err(|e| e.to_string())?.is_some())
}

#[derive(Serialize)]
struct LiveRecording {
    meeting_id: String,
    started_at: i64,
}

/// What is being recorded right now, if anything.
///
/// The floating panel opens into an existing meeting rather than starting one,
/// and the tray can start a recording that no window initiated — both need to
/// ask rather than assume.
#[tauri::command]
fn live_recording(state: State<App>) -> Result<Option<LiveRecording>, String> {
    Ok(state.recording.lock().map_err(|e| e.to_string())?.as_ref().map(|r| LiveRecording {
        meeting_id: r.meeting_id.clone(),
        started_at: r.started_at,
    }))
}

// ── notes typed during the meeting ─────────────────────────────────────────

#[tauri::command]
fn add_fragment(
    app: AppHandle,
    state: State<App>,
    meeting_id: String,
    at_secs: f64,
    text: String,
) -> Result<(), String> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Ok(());
    }
    with_store(&state, |store| {
        store
            .add_fragment(&meeting_id, &NoteFragment { at_secs, text: text.clone() })
            .map_err(|e| e.to_string())
    })?;
    // The main window and the floating panel write to the same meeting, so a
    // fragment typed in one has to appear in the other.
    let _ = app.emit("fragment:added", (&meeting_id, at_secs, &text));
    Ok(())
}

#[tauri::command]
fn fragments(state: State<App>, meeting_id: String) -> Result<Vec<NoteFragment>, String> {
    with_store(&state, |store| {
        store.fragments(&meeting_id).map_err(|e| e.to_string())
    })
}

/// Names a speaker in one meeting — for when the calendar could not say who
/// they were, or said wrongly.
#[tauri::command]
fn rename_speaker(
    app: AppHandle,
    state: State<App>,
    meeting_id: String,
    speaker: String,
    name: String,
) -> Result<(), String> {
    let name = name.trim();
    with_store(&state, |store| {
        store.name_speaker(&meeting_id, &speaker, name).map_err(|e| e.to_string())
    })?;
    let _ = app.emit("meeting:updated", meeting_id);
    Ok(())
}

/// Runs transcription again from the audio on disk, replacing the transcript.
/// For meetings processed by an older, buggier pipeline, and for any that
/// failed. Notes typed during the meeting are untouched.
#[tauri::command]
fn retranscribe(app: AppHandle, state: State<App>, meeting_id: String) -> Result<(), String> {
    let meeting = with_store(&state, |store| {
        let meeting = store
            .meeting(&meeting_id)
            .map_err(|e| e.to_string())?
            .ok_or("no such meeting")?;
        if matches!(meeting.status, Status::Recording | Status::Transcribing | Status::Enhancing) {
            return Err("this meeting is still being processed".into());
        }
        if meeting.audio_dir.is_empty() {
            return Err("the recordings for this meeting were deleted".into());
        }
        Ok(meeting)
    })?;
    with_store(&state, |store| {
        store.set_status(&meeting.id, Status::Transcribing).map_err(|e| e.to_string())
    })?;
    let _ = app.emit("meeting:updated", meeting_id.clone());
    // You just asked for this, so the calendar may ask too — it is the one
    // moment a permission prompt has someone to answer it.
    transcribe_in_background(app, meeting_id, true);
    Ok(())
}

// ── reads ──────────────────────────────────────────────────────────────────

#[tauri::command]
fn recent_meetings(state: State<App>, limit: i64) -> Result<Vec<Meeting>, String> {
    with_store(&state, |store| store.recent(limit).map_err(|e| e.to_string()))
}

#[tauri::command]
fn meeting(state: State<App>, meeting_id: String) -> Result<Option<Meeting>, String> {
    with_store(&state, |store| {
        store.meeting(&meeting_id).map_err(|e| e.to_string())
    })
}

#[tauri::command]
fn streams(state: State<App>, meeting_id: String) -> Result<Vec<Stream>, String> {
    with_store(&state, |store| {
        store.streams(&meeting_id).map_err(|e| e.to_string())
    })
}

#[tauri::command]
fn turns(state: State<App>, meeting_id: String, from: f64, to: f64) -> Result<Vec<Turn>, String> {
    with_store(&state, |store| {
        store.turns(&meeting_id, from, to).map_err(|e| e.to_string())
    })
}

#[tauri::command]
fn note(state: State<App>, meeting_id: String) -> Result<Option<String>, String> {
    with_store(&state, |store| {
        store.note(&meeting_id).map_err(|e| e.to_string())
    })
}

#[derive(Serialize)]
struct Hit {
    meeting_id: String,
    title: Option<String>,
    started_at: i64,
    snippet: String,
}

/// Full-text search over the written notes.
///
/// The index stores the snippet but not enough to render a result, so each hit
/// is joined back to its meeting here rather than in the frontend — a match can
/// be older than anything the sidebar has loaded.
#[tauri::command]
fn search(state: State<App>, query: String) -> Result<Vec<Hit>, String> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    with_store(&state, |store| {
        let hits = store.search(&query, 30).map_err(|e| e.to_string())?;
        Ok(hits
            .into_iter()
            .filter_map(|(meeting_id, snippet)| {
                let meeting = store.meeting(&meeting_id).ok().flatten()?;
                Some(Hit {
                    meeting_id,
                    title: meeting.title,
                    started_at: meeting.started_at,
                    snippet,
                })
            })
            .collect())
    })
}

// ── what is actually running ───────────────────────────────────────────────

#[derive(Serialize)]
struct SystemInfo {
    speech_model: String,
    speech_model_ready: bool,
    /// Empty when Ollama is not running — the note step is the only part that
    /// needs it, so everything else keeps working.
    llm_model: String,
    llm_available: bool,
    llm_installed: Vec<String>,
    segmentation_origin: String,
    embedding_origin: String,
    data_dir: String,
}

/// The engines behind each stage, named rather than implied.
///
/// Model provenance is a stated constraint on this project, not trivia, so the
/// origins are shown in the interface where they can be checked instead of
/// living only in a comment.
#[tauri::command]
fn system_info() -> SystemInfo {
    let installed = meetnote_llm::Ollama::installed("http://localhost:11434").unwrap_or_default();
    // The const rather than a loaded backend: reporting provenance should not
    // cost a model load.
    let origin = meetnote_diarize::speakrs_backend::DEFAULT_ORIGIN;
    SystemInfo {
        speech_model: MODEL_FILE.to_string(),
        speech_model_ready: model_path().exists(),
        llm_model: default_model(),
        llm_available: !installed.is_empty(),
        llm_installed: installed,
        segmentation_origin: origin.segmentation.to_string(),
        embedding_origin: origin.embedding.to_string(),
        data_dir: data_dir().to_string_lossy().to_string(),
    }
}

// ── permissions ────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Default)]
struct Permissions {
    #[serde(default)]
    microphone: String,
    #[serde(default)]
    calendar: String,
    #[serde(default)]
    system_audio: String,
}

/// What macOS has granted.
///
/// Recording this computer's audio has no status API, so it is answered the
/// only way that is actually true: by whether the last completed recording
/// carried any sound. A guess dressed as a status would be worse than saying
/// nothing.
#[tauri::command]
fn permissions(state: State<App>) -> Result<Permissions, String> {
    let mut reported = Command::new(sidecar()?)
        .arg("--permissions")
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|out| serde_json::from_slice::<Permissions>(&out.stdout).ok())
        .unwrap_or_default();

    reported.system_audio = with_store(&state, |store| {
        for meeting in store.recent(10).map_err(|e| e.to_string())? {
            let streams = store.streams(&meeting.id).map_err(|e| e.to_string())?;
            if let Some(system) = streams.iter().find(|s| s.label == "system") {
                // A stream that ran but recorded nothing is what a missing
                // grant looks like from here.
                return Ok(if system.is_silent() { "denied" } else { "granted" }.to_string());
            }
        }
        Ok("not-asked".to_string())
    })
    .unwrap_or_else(|_| "unknown".to_string());

    Ok(reported)
}


// ── the model ──────────────────────────────────────────────────────────────

/// Quantised GGML rather than fp16: ~547 MB instead of ~1.6 GB, which is the
/// difference between a fifteen-minute first run and a forty-minute one. If
/// accuracy on code-switched speech suffers, the fp16 build is a settings
/// change, not a rewrite.
const MODEL_FILE: &str = "ggml-large-v3-turbo-q5_0.bin";
const MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin";

fn model_path() -> PathBuf {
    data_dir().join("models").join(MODEL_FILE)
}

#[derive(Serialize)]
struct ModelStatus {
    ready: bool,
    megabytes: u64,
}

#[tauri::command]
fn model_status() -> ModelStatus {
    let size = std::fs::metadata(model_path()).map(|m| m.len()).unwrap_or(0);
    ModelStatus {
        // A partial file from an interrupted download is not a usable model.
        ready: size > 400_000_000,
        megabytes: size / 1_000_000,
    }
}

#[derive(Clone, Serialize)]
struct Progress {
    megabytes: u64,
    total_megabytes: u64,
    done: bool,
    error: Option<String>,
}

/// Downloads to a temporary name and renames on completion, so an interrupted
/// download can never be mistaken for a working model.
#[tauri::command]
fn download_model(app: AppHandle) {
    std::thread::spawn(move || {
        let report = |megabytes, total_megabytes, done, error| {
            let _ = app.emit("model:progress", Progress { megabytes, total_megabytes, done, error });
        };
        let path = model_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let partial = path.with_extension("partial");

        let result = (|| -> std::result::Result<(), String> {
            let client = reqwest::blocking::Client::builder()
                .timeout(None)
                .build()
                .map_err(|e| e.to_string())?;
            let mut response = client.get(MODEL_URL).send().map_err(|e| e.to_string())?;
            let total = response.content_length().unwrap_or(0);
            let mut file = std::fs::File::create(&partial).map_err(|e| e.to_string())?;

            let mut written: u64 = 0;
            let mut buffer = vec![0_u8; 1 << 20];
            let mut last_reported = 0_u64;
            loop {
                let read = std::io::Read::read(&mut response, &mut buffer).map_err(|e| e.to_string())?;
                if read == 0 {
                    break;
                }
                std::io::Write::write_all(&mut file, &buffer[..read]).map_err(|e| e.to_string())?;
                written += read as u64;
                // Report every few MB rather than every chunk — the UI only
                // needs to look alive, not to re-render a thousand times.
                if written - last_reported > 4_000_000 {
                    last_reported = written;
                    report(written / 1_000_000, total / 1_000_000, false, None);
                }
            }
            drop(file);
            std::fs::rename(&partial, &path).map_err(|e| e.to_string())?;
            Ok(())
        })();

        match result {
            Ok(()) => report(0, 0, true, None),
            Err(error) => {
                let _ = std::fs::remove_file(&partial);
                report(0, 0, true, Some(error));
            }
        }
    });
}

// ── transcription ──────────────────────────────────────────────────────────

/// Runs after capture. Whisper on the ANE is cheap enough to run immediately,
/// unlike the enhancement pass, which wants the machine to itself.
///
/// `may_ask` lets the calendar lookup show its permission prompt; true only when
/// the user started this.
fn transcribe_in_background(app: AppHandle, meeting_id: String, may_ask: bool) {
    std::thread::spawn(move || {
        let state = app.state::<App>();
        let model = model_path();
        if !model.exists() {
            return; // nothing to do until the model is downloaded
        }
        let Ok(_turn) = state.transcribing.lock() else { return };

        let prepared = with_store(&state, |store| {
            let meeting = store
                .meeting(&meeting_id)
                .map_err(|e| e.to_string())?
                .ok_or("meeting vanished")?;
            let mut streams = store.streams(&meeting_id).map_err(|e| e.to_string())?;

            // Recordings made before the sidecar measured its own clock carry
            // no check on their header, so check them against the meeting's:
            // a stream that ran the whole meeting but claims half of it was
            // read at the wrong rate. Corrected in the store, so this happens
            // once and the capture check shows the real length.
            if let Some(ended_at) = meeting.ended_at {
                let span = (ended_at - meeting.started_at) as f64;
                for stream in &mut streams {
                    if correct_rate(stream, span - stream.offset_secs) {
                        store.put_stream(&meeting_id, stream).map_err(|e| e.to_string())?;
                    }
                }
            }

            // The words you type are the names Whisper otherwise mangles, so
            // they are fed back as decoding hints. The tool gets better at your
            // vocabulary the more you use it, with nothing to configure.
            let mut vocab: Vec<String> = store
                .fragments(&meeting_id)
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|f| f.text)
                .collect();
            // Terms from earlier meetings too: "webhooks" came back as "wed
            // hooks" in a meeting where it was never typed, and the words were
            // sitting in the previous one.
            let seen: std::collections::HashSet<String> =
                vocab.iter().map(|t| t.to_lowercase()).collect();
            for term in store
                .recent_vocabulary(&meeting_id, 120)
                .map_err(|e| e.to_string())?
            {
                if !seen.contains(&term.to_lowercase()) {
                    vocab.push(term);
                }
            }
            Ok((meeting, streams, vocab))
        });
        let Ok((meeting, streams, vocab)) = prepared else { return };

        // Who was invited. Asked at record start; for meetings recorded before
        // that was kept, the calendar is asked about the time the meeting
        // began — silently unless the user asked for this run.
        let participants = backfill_participants(&state, &meeting, may_ask).unwrap_or_default();

        // A meeting's names are its strongest vocabulary: whisper hears "Asha"
        // far more reliably when told to expect it.
        let title = meeting.title.clone().unwrap_or_default();
        let mut vocab = vocab;
        let names: Vec<String> = participants
            .iter()
            .filter(|p| !p.is_me)
            .filter_map(|p| p.display_name(&title))
            .collect();
        for name in names.into_iter().rev() {
            vocab.insert(0, name);
        }

        // whisper's initial prompt is capped at 224 tokens and silently drops
        // the rest, so this meeting's own words go in first and older ones
        // fill what is left.
        let hint = if vocab.is_empty() {
            None
        } else {
            let mut room = 600usize;
            let mut kept: Vec<String> = Vec::new();
            for term in vocab {
                let cost = term.chars().count() + 2;
                if cost > room {
                    break;
                }
                room -= cost;
                kept.push(term);
            }
            Some(format!("This meeting mentions: {}.", kept.join(", ")))
        };

        let paths: Vec<(PathBuf, String, f64, u32)> = streams
            .iter()
            .filter(|s| !s.is_silent())
            .map(|s| {
                (
                    PathBuf::from(&meeting.audio_dir).join(&s.path),
                    if s.label == "mic" { "ME".to_string() } else { "THEM".to_string() },
                    s.offset_secs,
                    // The stored rate, not the header's: they differ exactly
                    // when the header was wrong.
                    s.sample_rate as u32,
                )
            })
            .collect();
        let inputs: Vec<meetnote_stt::Stream> = paths
            .iter()
            .map(|(path, speaker, offset, rate)| meetnote_stt::Stream {
                path,
                speaker,
                offset_secs: *offset,
                sample_rate: Some(*rate),
            })
            .collect();

        let system_audio = paths
            .iter()
            .find(|(_, speaker, _, _)| speaker == "THEM")
            .and_then(|(path, _, offset, rate)| {
                meetnote_stt::load_16k_mono(path, Some(*rate)).ok().map(|audio| (audio, *offset))
            });

        let outcome = meetnote_stt::transcribe(
            &inputs,
            &meetnote_stt::Options {
                model: &model,
                // Never force English: doing so translates non-English speech
                // instead of transcribing it, and the result reads as a
                // flawless transcript of words nobody said.
                language: None,
                vocab: hint.as_deref(),
            },
        );

        // Split THEM into individual speakers. Deliberately best-effort: a
        // transcript with one undifferentiated far end is far better than no
        // transcript, so a diarisation failure must never fail the meeting.
        let outcome = outcome.map(|mut turns| {
            if let Some((audio, offset)) = system_audio {
                match diarize_far_end(&audio, offset) {
                    Ok(spans) => meetnote_diarize::apply(&mut turns, &spans),
                    Err(error) => eprintln!("diarisation skipped: {error}"),
                }
            }
            name_far_end(&mut turns, &participants, &title);
            turns
        });

        let transcribed = with_store(&state, |store| {
            match outcome {
                // Nothing transcribed is a failure, not a finished meeting.
                // This is the state that hid a broken capture for weeks: the
                // meeting went green, the note was simply absent, and no error
                // was written anywhere.
                Ok(turns) if turns.is_empty() => {
                    let why = if inputs.is_empty() {
                        "no audio was captured — every stream was silent"
                    } else {
                        "the audio produced no transcript"
                    };
                    store.fail(&meeting_id, why).map_err(|e| e.to_string())?;
                    Ok(false)
                }
                Ok(turns) => {
                    store.put_turns(&meeting_id, &turns).map_err(|e| e.to_string())?;
                    store.set_status(&meeting_id, Status::Enhancing).map_err(|e| e.to_string())?;
                    Ok(true)
                }
                Err(error) => {
                    store.fail(&meeting_id, &error.to_string()).map_err(|e| e.to_string())?;
                    Ok(false)
                }
            }
        });
        let _ = app.emit("meeting:updated", meeting_id.clone());

        if transcribed == Ok(true) {
            write_note(app.clone(), meeting_id);
        }
    });
}

/// Puts names on the far end once diarisation has had its say.
///
/// When the calendar leaves only one person it can be, every far-end turn is
/// theirs — diarisation regularly splits a single remote voice into two or
/// three "speakers", and that is precisely what the calendar corrects. When
/// diarisation heard one voice and nobody can be named, it is "Them" rather
/// than "Speaker 1": a number implies there are others.
fn name_far_end(turns: &mut [Turn], participants: &[Participant], title: &str) {
    // Voices that hold a real share of what the far end said. On the 29 Oct
    // interview — one remote person — clustering split off a second and third
    // "speaker" made of nothing but "yeah", "mm-hmm" and "oh": 3% of the words.
    // Measured in words, not seconds: whisper stretches a backchannel over the
    // whole pause around it, and by time that same cluster looked like 15%.
    let mut held: std::collections::BTreeMap<&str, usize> = Default::default();
    for turn in turns.iter().filter(|t| t.speaker != "ME") {
        *held.entry(turn.speaker.as_str()).or_default() += turn.text.split_whitespace().count();
    }
    let total: usize = held.values().sum();
    let voices = held
        .iter()
        .filter(|(label, words)| **label != "THEM" && total > 0 && **words as f64 / total as f64 >= 0.15)
        .count()
        .max(1);
    let name = meetnote_core::people::the_other_person(participants, title, voices);

    for turn in turns.iter_mut().filter(|t| t.speaker != "ME") {
        match &name {
            Some(name) => {
                turn.speaker = "THEM-1".into();
                turn.speaker_name = Some(name.clone());
            }
            None if voices == 1 => turn.speaker = "THEM".into(),
            None => {}
        }
    }
}

/// Labels the far-end speakers, shifting spans onto the meeting timeline so
/// they line up with the turns they are labelling.
fn diarize_far_end(audio: &[f32], offset_secs: f64) -> meetnote_diarize::Result<Vec<meetnote_diarize::Span>> {
    use meetnote_diarize::{speakrs_backend::Speakrs, Backend};
    let backend = Speakrs::pretrained()?;
    let mut spans = backend.diarize(audio)?;
    for span in &mut spans {
        span.start_secs += offset_secs;
        span.end_secs += offset_secs;
    }
    Ok(spans)
}

/// The note. Runs after transcription rather than alongside it: a 20B model is
/// 13 GB of weights, and this machine has other things to do.
fn write_note(app: AppHandle, meeting_id: String) {
    let state = app.state::<App>();

    let material = with_store(&state, |store| {
        let turns = store.turns(&meeting_id, 0.0, f64::MAX).map_err(|e| e.to_string())?;
        let fragments = store.fragments(&meeting_id).map_err(|e| e.to_string())?;
        Ok((turns, fragments))
    });
    let Ok((turns, fragments)) = material else { return };

    let provider = meetnote_llm::Ollama::new(default_model());
    let mut written = String::new();
    let outcome = meetnote_llm::enhance(&provider, &turns, &fragments, &mut |token| {
        written.push_str(token);
        // Stream it: a note takes a minute or more, and a frozen window looks
        // like a hang rather than work.
        let _ = app.emit("note:token", token.to_string());
    });

    let _ = with_store(&state, |store| match outcome {
        Ok(Some(note)) => {
            store
                .put_note(&meeting_id, &note.body, &note.model, note.prompt_version, now())
                .map_err(|e| e.to_string())?;
            store.set_status(&meeting_id, Status::Done).map_err(|e| e.to_string())
        }
        // No fragments is not a failure — there was simply nothing to expand,
        // and a generic summary is the thing this deliberately does not write.
        Ok(None) => store.set_status(&meeting_id, Status::Done).map_err(|e| e.to_string()),
        Err(error) => store.fail(&meeting_id, &error.to_string()).map_err(|e| e.to_string()),
    });
    let _ = app.emit("meeting:updated", meeting_id);
}

/// Picks a pulled model, preferring the one the brief settled on.
fn default_model() -> String {
    let installed = meetnote_llm::Ollama::installed("http://localhost:11434").unwrap_or_default();
    installed
        .iter()
        .find(|name| name.starts_with("gpt-oss"))
        .or_else(|| installed.first())
        .cloned()
        .unwrap_or_else(|| "gpt-oss:20b".to_string())
}

// ── calendar ───────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct CalendarEvent {
    #[serde(default)]
    title: String,
    /// Names only, from sidecars that predate `participants`.
    #[serde(default)]
    attendees: Vec<String>,
    #[serde(default)]
    participants: Option<Vec<Participant>>,
}

impl CalendarEvent {
    fn participants(&self) -> Vec<Participant> {
        self.participants.clone().unwrap_or_else(|| {
            self.attendees
                .iter()
                .map(|name| Participant {
                    name: Some(name.clone()),
                    email: None,
                    is_me: false,
                    kind: "unknown".into(),
                    declined: false,
                })
                .collect()
        })
    }
}

/// Asks the sidecar what meeting is happening now, or was happening at `at`
/// (unix seconds). Entirely optional: no calendar grant means an untitled
/// meeting, never a failed one.
///
/// `may_ask` is whether the permission prompt may appear — only when the user
/// has just done something (started a recording, asked for a re-transcribe).
/// Either way the answer is bounded: a lookup that never returns must not hold
/// up the meeting it was only meant to decorate.
fn calendar_event(at: Option<i64>, may_ask: bool) -> Option<CalendarEvent> {
    let mut command = Command::new(sidecar().ok()?);
    command.arg("--calendar");
    if let Some(at) = at {
        command.args(["--at", &at.to_string()]);
    }
    if !may_ask {
        command.arg("--no-ask");
    }
    let mut child = command.stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let limit = std::time::Duration::from_secs(if may_ask { 60 } else { 10 });
    let started = std::time::Instant::now();
    while child.try_wait().ok()?.is_none() {
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let mut line = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut line).ok()?;
    let event: CalendarEvent = serde_json::from_str(line.trim()).ok()?;
    (!event.title.is_empty()).then_some(event)
}

/// Fills in who was invited for a meeting recorded before that was stored.
/// Only for meetings the calendar already named, so an ad-hoc call never
/// borrows the invitees of whatever happened to be scheduled.
///
/// The lookup runs outside the store lock and off the main thread: it can sit
/// behind a permission prompt for up to a minute.
fn backfill_participants(state: &App, meeting: &Meeting, may_ask: bool) -> Option<Vec<Participant>> {
    let known = with_store(state, |store| store.participants(&meeting.id).map_err(|e| e.to_string()));
    if let Ok(Some(known)) = known {
        return Some(known);
    }
    if meeting.source.as_deref() != Some("calendar") {
        return None;
    }
    let found = calendar_event(Some(meeting.started_at), may_ask)?.participants();
    let _ = with_store(state, |store| {
        store.set_participants(&meeting.id, &found).map_err(|e| e.to_string())
    });
    Some(found)
}

#[tauri::command]
fn rename_meeting(state: State<App>, meeting_id: String, title: String) -> Result<(), String> {
    let title = title.trim().to_string();
    if title.is_empty() {
        return Err("a meeting needs a name".into());
    }
    with_store(&state, |store| {
        store.set_title(&meeting_id, &title, None).map_err(|e| e.to_string())
    })
}

/// Deletes the recordings folder, but only one that belongs to meetnote.
///
/// The path comes out of the database, and this removes a directory tree, so it
/// is checked against the folder the app owns rather than trusted. A stored
/// path that points anywhere else is a bug, and the files stay put.
fn remove_recordings(audio_dir: &str) -> Result<(), String> {
    if audio_dir.is_empty() {
        return Ok(()); // already forgotten
    }
    let path = PathBuf::from(audio_dir);
    let root = data_dir().join("recordings");
    if !path.starts_with(&root) || path == root {
        return Err(format!("refusing to delete {audio_dir}: outside {}", root.display()));
    }
    match std::fs::remove_dir_all(&path) {
        Ok(()) => Ok(()),
        // Already gone is the state we wanted.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Deletes a meeting: rows first, then its audio.
#[tauri::command]
fn delete_meeting(app: AppHandle, state: State<App>, meeting_id: String) -> Result<(), String> {
    // A meeting still being recorded or transcribed has a pipeline holding it.
    if let Some(status) = with_store(&state, |store| {
        Ok(store.meeting(&meeting_id).map_err(|e| e.to_string())?.map(|m| m.status))
    })? {
        if matches!(status, Status::Recording | Status::Transcribing | Status::Enhancing) {
            return Err("this meeting is still being processed".into());
        }
    }
    let audio_dir =
        with_store(&state, |store| store.delete_meeting(&meeting_id).map_err(|e| e.to_string()))?;
    remove_recordings(&audio_dir)?;
    let _ = app.emit("meeting:updated", meeting_id);
    Ok(())
}

/// Deletes only the audio, keeping the transcript and the note.
#[tauri::command]
fn delete_recordings(app: AppHandle, state: State<App>, meeting_id: String) -> Result<(), String> {
    let audio_dir =
        with_store(&state, |store| store.forget_audio(&meeting_id).map_err(|e| e.to_string()))?;
    remove_recordings(&audio_dir)?;
    let _ = app.emit("meeting:updated", meeting_id);
    Ok(())
}

// ── call detection ─────────────────────────────────────────────────────────

/// Runs the sidecar in watch mode for the app's lifetime. It records nothing —
/// it reports when a call starts so the UI can offer a one-click recording
/// instead of relying on you to remember.
fn watch_for_calls(app: AppHandle) {
    let Ok(path) = sidecar() else { return };
    std::thread::spawn(move || {
        let Ok(mut child) = Command::new(path)
            .arg("--watch-calls")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else {
            return;
        };
        let Some(stdout) = child.stdout.take() else { return };
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if event.get("type").and_then(|t| t.as_str()) != Some("call") {
                continue;
            }
            let call = CallEvent {
                state: event["state"].as_str().unwrap_or("ended").to_string(),
                name: event["name"].as_str().unwrap_or("a call").to_string(),
                app: event["app"].as_str().unwrap_or_default().to_string(),
            };
            let _ = app.emit("call:changed", &call);
            if call.state == "started" {
                call_started(&app, call);
            } else {
                call_ended(&app);
            }
        }
    });
}

fn is_live(app: &AppHandle) -> bool {
    app.state::<App>().recording.lock().map(|r| r.is_some()).unwrap_or(false)
}

/// Offers to record where you will see it: the banner in the main window is
/// invisible while you are looking at the call, which is exactly when it fires.
fn call_started(app: &AppHandle, call: CallEvent) {
    let state = app.state::<App>();
    let fresh = {
        let Ok(mut current) = state.call.lock() else { return };
        // A call is several audio processes; the watcher reports each handover,
        // and only the first one is news.
        let fresh = current.current.is_none();
        current.current = Some(call.clone());
        current.stop_at = None;
        if fresh {
            current.dismissed = false;
        }
        fresh
    };
    if !fresh || is_live(app) {
        return;
    }
    show_panel(app, PanelMode::Prompt);
    // The panel can sit behind a full-screen call on another Space; the
    // notification can't.
    use tauri_plugin_notification::NotificationExt;
    let _ = app
        .notification()
        .builder()
        .title(format!("{} call started", call.name))
        .body("Record it from the meetnote panel.")
        .show();
}

fn call_ended(app: &AppHandle) {
    let state = app.state::<App>();
    if let Ok(mut current) = state.call.lock() {
        current.current = None;
        current.dismissed = false;
    }
    if !is_live(app) {
        hide_panel(app);
        return;
    }
    let Some(meeting_id) = state.recording.lock().ok().and_then(|r| r.as_ref().map(|r| r.meeting_id.clone()))
    else {
        return;
    };
    let deadline = std::time::Instant::now() + STOP_AFTER_CALL;
    if let Ok(mut current) = state.call.lock() {
        current.stop_at = Some(deadline);
    }
    let _ = app.emit("call:stopping", STOP_AFTER_CALL.as_secs());
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(STOP_AFTER_CALL);
        let state = app.state::<App>();
        // Anything since — a rejoin, "keep recording", a manual stop, a new
        // call ending later — has replaced or cleared this deadline.
        let due = state.call.lock().map(|c| c.stop_at == Some(deadline)).unwrap_or(false);
        let same = state
            .recording
            .lock()
            .map(|r| r.as_ref().is_some_and(|r| r.meeting_id == meeting_id))
            .unwrap_or(false);
        if due && same {
            let _ = stop_recording(app.clone(), state);
        }
    });
}

#[derive(Serialize)]
struct CallPrompt {
    name: String,
    /// Seconds until a recording whose call has ended stops by itself.
    stops_in: Option<u64>,
}

/// What the panel should offer right now: a call to record, or a recording
/// about to stop because its call ended.
#[tauri::command]
fn current_call(state: State<App>) -> Result<Option<CallPrompt>, String> {
    let call = state.call.lock().map_err(|e| e.to_string())?;
    let stops_in = call
        .stop_at
        .map(|at| at.saturating_duration_since(std::time::Instant::now()).as_secs());
    Ok(match (&call.current, stops_in) {
        (Some(c), _) if !call.dismissed => Some(CallPrompt { name: c.name.clone(), stops_in: None }),
        (_, Some(secs)) => Some(CallPrompt { name: String::new(), stops_in: Some(secs) }),
        _ => None,
    })
}

#[tauri::command]
fn dismiss_call(app: AppHandle, state: State<App>) -> Result<(), String> {
    state.call.lock().map_err(|e| e.to_string())?.dismissed = true;
    if !is_live(&app) {
        hide_panel(&app);
    }
    Ok(())
}

#[tauri::command]
fn keep_recording(state: State<App>) -> Result<(), String> {
    state.call.lock().map_err(|e| e.to_string())?.stop_at = None;
    Ok(())
}

// ── the floating panel ─────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum PanelMode {
    /// Just the question, small enough not to cover a face.
    Prompt,
    /// The meters and the note log, while recording.
    Notes,
}

/// A small always-on-top window that sits over the call while you type.
///
/// The main window is a poor place to take notes during a meeting: you would be
/// tabbing away from the person talking. This is deliberately undecorated,
/// narrow, and created unfocused so appearing does not steal the keyboard from
/// the call — you click into it when you want it.
fn show_panel(app: &AppHandle, mode: PanelMode) {
    let (width, height) = match mode {
        PanelMode::Prompt => (340.0, 124.0),
        PanelMode::Notes => (360.0, 460.0),
    };
    if let Some(existing) = app.get_webview_window("panel") {
        let _ = existing.set_size(tauri::LogicalSize::new(width, height));
        let _ = existing.show();
        if mode == PanelMode::Notes {
            let _ = existing.set_focus();
        }
        return;
    }
    // Top-right, clear of where video calls put faces.
    let (x, y) = app
        .primary_monitor()
        .ok()
        .flatten()
        .map(|m| (m.size().to_logical::<f64>(m.scale_factor()).width - width - 24.0, 48.0))
        .unwrap_or((48.0, 48.0));
    let built = WebviewWindowBuilder::new(app, "panel", WebviewUrl::App("index.html#panel".into()))
        .title("meetnote")
        .inner_size(width, height)
        .min_inner_size(300.0, 110.0)
        .position(x, y)
        .always_on_top(true)
        .decorations(false)
        .resizable(true)
        .focused(false)
        .skip_taskbar(true)
        .build();
    if let Ok(panel) = built {
        float_over_every_desktop(&panel);
    }
}

/// Otherwise the panel opens on whichever desktop meetnote's main window is
/// on, and never over a full-screen call, which is its own desktop. Only a
/// non-activating panel may sit there, so the window becomes one: the same
/// move anarlog and other call overlays make.
#[cfg(target_os = "macos")]
fn float_over_every_desktop(panel: &tauri::WebviewWindow) {
    use objc2::runtime::AnyObject;
    use objc2::ClassType;
    use objc2_app_kit::{NSPanel, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask};

    let target = panel.clone();
    let _ = panel.run_on_main_thread(move || {
        let Ok(pointer) = target.ns_window() else { return };
        // SAFETY: Tauri hands out the live NSWindow it owns, AppKit is only
        // touched here on the main thread, and the new class is an NSWindow
        // subclass that adds no storage.
        let window = unsafe { &*(pointer as *const NSWindow) };
        let original = unsafe { AnyObject::set_class(window.as_ref(), FloatingPanel::class()) };
        let _ = TAO_WINDOW_CLASS.set(original);
        let panel = unsafe { &*(pointer as *const NSPanel) };
        panel.setStyleMask(panel.styleMask() | NSWindowStyleMask::NonactivatingPanel);
        panel.setFloatingPanel(true);
        panel.setHidesOnDeactivate(false);
        panel.setCollectionBehavior(
            panel.collectionBehavior()
                | NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        // Placed before it could join every desktop, so place it again.
        panel.orderFrontRegardless();
    });
}

/// The class Tauri created the panel with. Its cleanup code expects that class,
/// and crashes the app if it finds the panel's instead, so it goes back on
/// before the window is destroyed.
#[cfg(target_os = "macos")]
static TAO_WINDOW_CLASS: std::sync::OnceLock<&'static objc2::runtime::AnyClass> = std::sync::OnceLock::new();

#[cfg(target_os = "macos")]
fn unfloat(panel: &tauri::WebviewWindow) {
    let Some(&original) = TAO_WINDOW_CLASS.get() else { return };
    let restore = move |target: &tauri::WebviewWindow| {
        let Ok(pointer) = target.ns_window() else { return };
        // SAFETY: as in `float_over_every_desktop`, in reverse.
        let window = unsafe { &*(pointer as *const objc2::runtime::AnyObject) };
        unsafe { objc2::runtime::AnyObject::set_class(window, original) };
    };
    // On quit this is already the main thread, and a queued job would never
    // run. Otherwise it is queued ahead of the close that follows it.
    if objc2::MainThreadMarker::new().is_some() {
        restore(panel);
    } else {
        let target = panel.clone();
        let _ = panel.run_on_main_thread(move || restore(&target));
    }
}

#[cfg(target_os = "macos")]
objc2::define_class!(
    // A borderless panel refuses the keyboard unless it says otherwise, and
    // the notes field needs it.
    #[unsafe(super(objc2_app_kit::NSPanel, objc2_app_kit::NSWindow, objc2_app_kit::NSResponder, objc2::runtime::NSObject))]
    #[thread_kind = objc2::MainThreadOnly]
    #[name = "MeetnoteFloatingPanel"]
    struct FloatingPanel;

    impl FloatingPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }
    }
);

#[cfg(not(target_os = "macos"))]
fn float_over_every_desktop(panel: &tauri::WebviewWindow) {
    let _ = panel.set_visible_on_all_workspaces(true);
}

fn hide_panel(app: &AppHandle) {
    if let Some(panel) = app.get_webview_window("panel") {
        #[cfg(target_os = "macos")]
        unfloat(&panel);
        let _ = panel.close();
    }
}

#[tauri::command]
fn close_panel(app: AppHandle) {
    hide_panel(&app);
}

/// Keeps the tray label honest about whether anything is being recorded.
fn refresh_tray(app: &AppHandle, recording: bool) {
    if let Some(item) = app.try_state::<TrayState>() {
        let _ = item.toggle.set_text(if recording { "Stop and write it up" } else { "Start recording" });
    }
}

struct TrayState {
    toggle: MenuItem<tauri::Wry>,
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let toggle = MenuItem::with_id(app, "toggle", "Start recording", true, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "Open meetnote", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&toggle, &open, &quit])?;

    app.manage(TrayState { toggle: toggle.clone() });

    TrayIconBuilder::with_id("meetnote")
        .icon(app.default_window_icon().unwrap().clone())
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "toggle" => {
                let app = app.clone();
                // Recording work must not run on the menu thread.
                std::thread::spawn(move || {
                    let state = app.state::<App>();
                    let recording = state.recording.lock().map(|r| r.is_some()).unwrap_or(false);
                    if recording {
                        let _ = stop_recording(app.clone(), state);
                    } else {
                        let _ = start_recording(app.clone(), state);
                    }
                });
            }
            "open" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "quit" => {
                hide_panel(app);
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;
    Ok(())
}

// ── setup ──────────────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .manage(App::default())
        .setup(|app| {
            let dir = data_dir();
            std::fs::create_dir_all(&dir)?;
            let store = Store::open(dir.join("meetnote.db"))?;

            build_tray(app.handle())?;
            watch_for_calls(app.handle().clone());

            // Anything still marked `recording` means the app died mid-meeting;
            // anything transcribing or enhancing means it quit mid-processing.
            // The audio is on disk either way. These used to be marked
            // "transcribing" and then never actually transcribed.
            let mut store = store;
            for stranded in store.recover_interrupted()? {
                let dir = PathBuf::from(&stranded.audio_dir);
                let ended = std::fs::metadata(dir.join("capture.json"))
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64);
                match ended {
                    Some(ended_at) => {
                        if let Err(error) = ingest_capture(&mut store, &stranded.id, &dir, ended_at) {
                            store.fail(&stranded.id, &error)?;
                        }
                    }
                    // The sidecar died before writing its metadata, so the wav
                    // headers were never finalised either.
                    None => store.fail(&stranded.id, "the app stopped before the recording was saved")?,
                }
            }
            // Ingesting marks a recovered meeting `transcribing`, so this picks
            // those up along with any whose processing was cut off.
            let resume = store.unfinished()?;

            *app.state::<App>().store.lock().unwrap() = Some(store);
            for meeting in resume {
                transcribe_in_background(app.handle().clone(), meeting.id, false);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_recording,
            stop_recording,
            is_recording,
            add_fragment,
            fragments,
            recent_meetings,
            meeting,
            streams,
            turns,
            note,
            search,
            model_status,
            download_model,
            rename_meeting,
            delete_meeting,
            delete_recordings,
            close_panel,
            live_recording,
            current_call,
            dismiss_call,
            keep_recording,
            permissions,
            system_info,
            rename_speaker,
            retranscribe,
        ])
        .build(tauri::generate_context!())
        .expect("error while building meetnote")
        .run(|app, event| {
            // Quitting from the app menu or the dock destroys the panel too.
            if let tauri::RunEvent::ExitRequested { .. } = event {
                hide_panel(app);
            }
        });
}
