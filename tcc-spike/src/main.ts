import { invoke } from "@tauri-apps/api/core";

interface CaptureResult {
  ok: boolean;
  capture_json: string | null;
  log: string;
  sidecar_path: string;
}

const verdictEl = document.querySelector<HTMLDivElement>("#verdict")!;
const detailEl = document.querySelector<HTMLPreElement>("#detail")!;
const buttonEl = document.querySelector<HTMLButtonElement>("#run")!;

/** The whole spike in one function: did the sidecar get real samples, or zeros? */
function verdict(result: CaptureResult): { text: string; pass: boolean } {
  if (!result.capture_json) {
    return { text: "no capture.json — the sidecar did not run", pass: false };
  }
  const streams = JSON.parse(result.capture_json).streams as Array<{
    label: string;
    silent: boolean;
  }>;
  const silent = streams.filter((s) => s.silent).map((s) => s.label);

  // The app speaks during capture, so the system stream has a known source.
  // A silent system stream therefore means the tap was refused — not merely
  // that nothing happened to be playing.
  if (silent.length === streams.length) {
    return {
      text: "ALL STREAMS SILENT — the grant did not survive the sidecar hop",
      pass: false,
    };
  }
  if (silent.includes("system")) {
    return {
      text: "SYSTEM SILENT despite the app speaking — the tap was refused (Privacy & Security → Audio Recording)",
      pass: false,
    };
  }
  if (silent.includes("mic")) {
    return {
      text: "MIC SILENT — enable Privacy & Security → Microphone (system audio is working)",
      pass: false,
    };
  }
  return {
    text: "REAL AUDIO ON BOTH STREAMS — the sidecar inherited the app's grants",
    pass: true,
  };
}

buttonEl.addEventListener("click", async () => {
  buttonEl.disabled = true;
  verdictEl.textContent = "recording 5s — make some noise…";
  verdictEl.className = "";
  detailEl.textContent = "";

  try {
    const result = await invoke<CaptureResult>("run_capture", { seconds: 5 });
    const { text, pass } = verdict(result);
    verdictEl.textContent = text;
    verdictEl.className = pass ? "pass" : "fail";
    detailEl.textContent = [
      `sidecar: ${result.sidecar_path}`,
      "",
      result.capture_json ?? "(no capture.json)",
      "",
      result.log,
    ].join("\n");
  } catch (error) {
    verdictEl.textContent = "invoke failed";
    verdictEl.className = "fail";
    detailEl.textContent = String(error);
  } finally {
    buttonEl.disabled = false;
  }
});
