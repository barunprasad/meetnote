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

use meetnote_core::{Meeting, NoteFragment, Status, Store, Stream, Turn};
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
    /// Held from the start of the meeting so speakers can be named once
    /// diarisation has produced them.
    attendees: Mutex<Vec<String>>,
}

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
    if let Some(event) = calendar_now() {
        let _ = with_store(&state, |store| {
            store
                .set_title(&meeting_id, &event.title, Some("calendar"))
                .map_err(|e| e.to_string())
        });
        *state.attendees.lock().map_err(|e| e.to_string())? = event.attendees;
    }

    *state.recording.lock().map_err(|e| e.to_string())? = Some(Recording {
        meeting_id: meeting_id.clone(),
        child,
        started_at,
    });
    show_panel(&app);
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
    let meta = std::fs::read_to_string(audio_dir.join("capture.json"))
        .map_err(|e| format!("capture produced no metadata: {e}"))?;
    let parsed: serde_json::Value = serde_json::from_str(&meta).map_err(|e| e.to_string())?;
    let streams: Vec<SidecarStream> =
        serde_json::from_value(parsed["streams"].clone()).map_err(|e| e.to_string())?;

    let mut stored = Vec::new();
    with_store(&state, |store| {
        for s in &streams {
            // The sidecar measures the loudest sample; keep it as a level so the
            // capture check can show what was heard rather than a boolean. Older
            // sidecars reported only `silent`, so fall back to the floor for
            // those — but never leave a healthy stream without a level, which
            // used to make every stream look silent and skip transcription.
            let peak = s.peak_dbfs.or(if s.silent { Some(-120.0) } else { None });
            store
                .put_stream(
                    &meeting_id,
                    &Stream {
                        label: s.label.clone(),
                        path: s.path.clone(),
                        sample_rate: s.sample_rate,
                        channels: s.channels,
                        offset_secs: s.offset_seconds,
                        duration_secs: Some(s.duration_seconds),
                        peak_dbfs: peak,
                    },
                )
                .map_err(|e| e.to_string())?;
            stored.push(StoredStream {
                label: s.label.clone(),
                duration_secs: s.duration_seconds,
                peak_dbfs: peak,
                silent: s.silent,
            });
        }
        store
            .finish_recording(&meeting_id, now())
            .map_err(|e| e.to_string())
    })?;

    transcribe_in_background(app, meeting_id.clone());
    Ok(Captured { meeting_id, streams: stored })
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
fn transcribe_in_background(app: AppHandle, meeting_id: String) {
    std::thread::spawn(move || {
        let state = app.state::<App>();
        let model = model_path();
        if !model.exists() {
            return; // nothing to do until the model is downloaded
        }

        let prepared = with_store(&state, |store| {
            let streams = store.streams(&meeting_id).map_err(|e| e.to_string())?;
            let audio_dir = store
                .meeting(&meeting_id)
                .map_err(|e| e.to_string())?
                .map(|m| m.audio_dir)
                .ok_or("meeting vanished")?;
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
            Ok((streams, audio_dir, vocab))
        });
        let Ok((streams, audio_dir, vocab)) = prepared else { return };

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

        let paths: Vec<(PathBuf, String, f64)> = streams
            .iter()
            .filter(|s| !s.is_silent())
            .map(|s| {
                (
                    PathBuf::from(&audio_dir).join(&s.path),
                    if s.label == "mic" { "ME".to_string() } else { "THEM".to_string() },
                    s.offset_secs,
                )
            })
            .collect();
        let inputs: Vec<meetnote_stt::Stream> = paths
            .iter()
            .map(|(path, speaker, offset)| meetnote_stt::Stream {
                path,
                speaker,
                offset_secs: *offset,
            })
            .collect();

        let system_audio = paths
            .iter()
            .find(|(_, speaker, _)| speaker == "THEM")
            .and_then(|(path, _, offset)| {
                meetnote_stt::load_16k_mono(path).ok().map(|audio| (audio, *offset))
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
            turns
        });

        let attendees = state.attendees.lock().map(|a| a.clone()).unwrap_or_default();
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
                    name_speakers_from_calendar(store, &meeting_id, &attendees);
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
            write_note(app, meeting_id);
        }
    });
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
    #[serde(default)]
    attendees: Vec<String>,
    #[serde(default)]
    in_progress: bool,
}

/// Asks the sidecar what meeting is happening now. Entirely optional: no
/// calendar grant means an untitled meeting, never a failed one.
fn calendar_now() -> Option<CalendarEvent> {
    let output = Command::new(sidecar().ok()?)
        .arg("--calendar")
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&output.stdout);
    let event: CalendarEvent = serde_json::from_str(line.trim()).ok()?;
    (!event.title.is_empty()).then_some(event)
}

/// Attendee names become the far-end speakers. The count is what makes this
/// safe to apply automatically: with exactly one other invitee there is only
/// one person `THEM-1` can be, so the mapping is certain rather than a guess.
/// Beyond that the names are recorded on the meeting and left for the user.
fn name_speakers_from_calendar(store: &Store, meeting_id: &str, attendees: &[String]) {
    let others: Vec<&String> = attendees.iter().collect();
    if others.len() == 1 {
        let _ = store.name_speaker(meeting_id, "THEM-1", others[0]);
        let _ = store.name_speaker(meeting_id, "THEM", others[0]);
    }
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
            let _ = app.emit(
                "call:changed",
                CallEvent {
                    state: event["state"].as_str().unwrap_or("ended").to_string(),
                    name: event["name"].as_str().unwrap_or("a call").to_string(),
                    app: event["app"].as_str().unwrap_or_default().to_string(),
                },
            );
        }
    });
}

// ── the floating panel ─────────────────────────────────────────────────────

/// A small always-on-top window that sits over the call while you type.
///
/// The main window is a poor place to take notes during a meeting: you would be
/// tabbing away from the person talking. This is deliberately undecorated,
/// narrow, and created unfocused so appearing does not steal the keyboard from
/// the call — you click into it when you want it.
fn show_panel(app: &AppHandle) {
    if let Some(existing) = app.get_webview_window("panel") {
        let _ = existing.show();
        let _ = existing.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "panel", WebviewUrl::App("index.html#panel".into()))
        .title("meetnote")
        .inner_size(360.0, 460.0)
        .min_inner_size(300.0, 260.0)
        .always_on_top(true)
        .decorations(false)
        .resizable(true)
        .focused(false)
        .skip_taskbar(true)
        .build();
}

fn hide_panel(app: &AppHandle) {
    if let Some(panel) = app.get_webview_window("panel") {
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
            "quit" => app.exit(0),
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
        .manage(App::default())
        .setup(|app| {
            let dir = data_dir();
            std::fs::create_dir_all(&dir)?;
            let store = Store::open(dir.join("meetnote.db"))?;

            build_tray(app.handle())?;
            watch_for_calls(app.handle().clone());

            // Anything still marked `recording` means the app died mid-meeting.
            // The audio is on disk, so re-queue it rather than stranding it.
            for stranded in store.recover_interrupted()? {
                store.set_status(&stranded.id, Status::Transcribing)?;
            }

            *app.state::<App>().store.lock().unwrap() = Some(store);
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
            permissions,
            system_info,
        ])
        .run(tauri::generate_context!())
        .expect("error while running meetnote");
}
