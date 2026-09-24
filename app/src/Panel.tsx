// The floating panel: a small always-on-top window for typing during a call.
//
// The main window is the wrong place to take notes mid-meeting — using it means
// tabbing away from the person talking. This sits over Zoom, stays narrow
// enough not to cover a face, and does nothing but take fragments.

import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { clock, FragmentLog, LiveRecording, Meter, spokenClock, useNoteTaking } from "./shared";
import "./styles.css";

function Notes({ meetingId, startedAtMs }: { meetingId: string; startedAtMs: number }) {
  const { elapsed, level, fragments, commit } = useNoteTaking(meetingId, startedAtMs);

  return (
    <>
      <header className="panel-head" data-tauri-drag-region>
        <span className="panel-state" data-tauri-drag-region>
          <span className="rec-dot" aria-hidden="true" />
          Recording
        </span>
        <span className="panel-clock" data-tauri-drag-region aria-label={`${spokenClock(elapsed)} elapsed`}>
          {clock(elapsed)}
        </span>
        <button className="button stop small" onClick={() => invoke("stop_recording")}>
          <span className="stop-square" aria-hidden="true" />
          Stop
        </button>
      </header>

      <div className="panel-body">
        <div className="panel-meters">
          <Meter label="You" dbfs={level.mic_dbfs} you />
          <Meter label="Them" dbfs={level.system_dbfs} />
        </div>
        <FragmentLog fragments={fragments} elapsed={elapsed} commit={commit} autoFocus />
      </div>
    </>
  );
}

export default function Panel() {
  // The panel is opened by the backend when recording starts, but it can also
  // be reloaded mid-meeting, so it asks rather than being told.
  const [live, setLive] = useState<LiveRecording | null | "unknown">("unknown");

  useEffect(() => {
    invoke<LiveRecording | null>("live_recording").then(setLive);
    const stopped = listen("recording:stopped", () => setLive(null));
    return () => {
      stopped.then((off) => off());
    };
  }, []);

  if (live === "unknown") return <div className="floating" />;

  return (
    <main className="floating" aria-label="Meeting notes">
      {live ? (
        // `started_at` is unix seconds from the backend; the clock counts in ms.
        <Notes meetingId={live.meeting_id} startedAtMs={live.started_at * 1000} />
      ) : (
        <header className="panel-head" data-tauri-drag-region>
          <span data-tauri-drag-region>Not recording</span>
          <button className="button small" onClick={() => invoke("close_panel")}>
            Close
          </button>
        </header>
      )}
    </main>
  );
}
