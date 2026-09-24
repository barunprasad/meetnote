// TCC spike — does a Swift sidecar spawned by a Tauri app inherit the app's
// audio-capture grant?
//
// The spike proved a bare CLI binary cannot obtain that grant: macOS attributes
// the request to the launching terminal, Warp never surfaced a prompt, and the
// capture came back as a full-length file of digital silence with no error. It
// only worked from inside a signed .app launched by `open`.
//
// This adds one more hop — app -> spawned sidecar — and asks whether the grant
// still holds. If peak_dbfs comes back as real audio, the architecture in
// ARCHITECTURE.md is sound. If it is silent, capture has to move into the Rust
// process via FFI, or the app goes native Swift after all.

use std::path::PathBuf;
use std::process::Command;

use serde::Serialize;

#[derive(Serialize)]
struct CaptureResult {
    ok: bool,
    /// Raw capture.json, so the UI can show exactly what the sidecar recorded.
    capture_json: Option<String>,
    /// The sidecar's own console output — it prints the tap diagnostics.
    log: String,
    sidecar_path: String,
}

/// Sidecars live beside the host binary: `Contents/MacOS/` in a bundled app,
/// `target/debug/` under `tauri dev`. Tauri strips the target-triple suffix
/// when bundling, so the name differs between the two.
fn sidecar_path() -> Result<PathBuf, String> {
    let dir = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .ok_or("no parent dir")?
        .to_path_buf();

    for name in [
        "meetnote-capture",
        "meetnote-capture-aarch64-apple-darwin",
    ] {
        let candidate = dir.join(name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(format!("no capture sidecar beside {}", dir.display()))
}

#[tauri::command]
fn run_capture(seconds: u32) -> Result<CaptureResult, String> {
    let sidecar = sidecar_path()?;
    let out_dir = std::env::temp_dir().join(format!("tcc-spike-{}", std::process::id()));

    let mut child = Command::new(&sidecar)
        .args([
            "--duration",
            &seconds.to_string(),
            "--out",
            &out_dir.to_string_lossy(),
        ])
        .spawn()
        .map_err(|e| format!("failed to spawn {}: {e}", sidecar.display()))?;

    // Play known audio while the tap runs. Without this the test cannot tell a
    // denied tap from a working tap with nothing playing — both are a file of
    // zeros. Making the spike produce its own sound removes the ambiguity: if
    // the system stream is silent now, the tap really was refused.
    std::thread::sleep(std::time::Duration::from_millis(700));
    let _ = Command::new("say")
        .args(["-r", "180", "testing system audio capture, one two three"])
        .status();

    let output = child
        .wait_with_output()
        .map_err(|e| format!("capture failed: {e}"))?;

    let capture_json = std::fs::read_to_string(out_dir.join("capture.json")).ok();

    // capture.swift redirects its console to <out>/capture.log whenever stdout
    // is not a tty — which is always, when spawned with a pipe. Read the file,
    // or the tap diagnostics are invisible here.
    let mut log = std::fs::read_to_string(out_dir.join("capture.log")).unwrap_or_default();
    log.push_str(&String::from_utf8_lossy(&output.stdout));
    log.push_str(&String::from_utf8_lossy(&output.stderr));

    Ok(CaptureResult {
        ok: capture_json.is_some(),
        capture_json,
        log,
        sidecar_path: sidecar.display().to_string(),
    })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![run_capture])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
