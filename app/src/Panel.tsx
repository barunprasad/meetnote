// The floating panel: a small always-on-top window for typing during a call.
//
// The main window is the wrong place to take notes mid-meeting — using it means
// tabbing away from the person talking. This sits over Zoom, stays narrow
// enough not to cover a face, and does nothing but take fragments.

import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  CallEvent,
  clock,
  FragmentLog,
  LiveRecording,
  Meter,
  spokenClock,
  useNoteTaking,
} from "./shared";
import "./styles.css";

interface CallPrompt {
  name: string;
  stops_in: number | null;
}

// The question the panel pops up to ask when a call starts.
function Offer({ name }: { name: string }) {
  return (
    <section className="offer" data-tauri-drag-region aria-labelledby="offer-title">
      <p id="offer-title" className="offer-title" data-tauri-drag-region>
        <span className="rec-dot" aria-hidden="true" />
        {name} call started
      </p>
      <p className="offer-text" data-tauri-drag-region>
        Record this meeting? Nothing is recorded unless you choose to.
      </p>
      <div className="offer-actions">
        <button className="button small" onClick={() => invoke("dismiss_call")}>
          Not now
        </button>
        <button className="button small record" onClick={() => invoke("start_recording")}>
          Record
        </button>
      </div>
    </section>
  );
}

// Counts down to the automatic stop after the call ends, with a way out for
// a meeting that carries on in the room.
function Stopping({ seconds, onKeep }: { seconds: number; onKeep: () => void }) {
  const [left, setLeft] = useState(seconds);
  useEffect(() => {
    const end = Date.now() + seconds * 1000;
    const tick = setInterval(() => setLeft(Math.max(0, Math.round((end - Date.now()) / 1000))), 500);
    return () => clearInterval(tick);
  }, [seconds]);

  return (
    <div className="stopping" role="status">
      <span>Call ended. Stopping in {left}s</span>
      <button
        className="button small"
        onClick={() => invoke("keep_recording").then(onKeep)}
      >
        Keep recording
      </button>
    </div>
  );
}

function Notes({ meetingId, startedAtMs }: { meetingId: string; startedAtMs: number }) {
  const { elapsed, level, fragments, commit } = useNoteTaking(meetingId, startedAtMs);
  const [stopsIn, setStopsIn] = useState<number | null>(null);

  useEffect(() => {
    invoke<CallPrompt | null>("current_call").then((c) => setStopsIn(c?.stops_in ?? null));
    const stopping = listen<number>("call:stopping", (event) => setStopsIn(event.payload));
    // Rejoining cancels the stop on the backend; mirror it here.
    const changed = listen<CallEvent>("call:changed", (event) => {
      if (event.payload.state === "started") setStopsIn(null);
    });
    return () => {
      stopping.then((off) => off());
      changed.then((off) => off());
    };
  }, []);

  return (
    <>
      {stopsIn !== null && <Stopping seconds={stopsIn} onKeep={() => setStopsIn(null)} />}
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
  const [call, setCall] = useState<string | null>(null);

  useEffect(() => {
    invoke<LiveRecording | null>("live_recording").then(setLive);
    invoke<CallPrompt | null>("current_call").then((c) => setCall(c && c.name ? c.name : null));
    const started = listen("recording:started", () =>
      invoke<LiveRecording | null>("live_recording").then(setLive),
    );
    const stopped = listen("recording:stopped", () => setLive(null));
    const changed = listen<CallEvent>("call:changed", (event) =>
      setCall(event.payload.state === "started" ? event.payload.name : null),
    );
    return () => {
      started.then((off) => off());
      stopped.then((off) => off());
      changed.then((off) => off());
    };
  }, []);

  if (live === "unknown") return <div className="floating" />;

  return (
    <main className="floating" aria-label={live ? "Meeting notes" : "Record this meeting?"}>
      {live ? (
        // `started_at` is unix seconds from the backend; the clock counts in ms.
        <Notes meetingId={live.meeting_id} startedAtMs={live.started_at * 1000} />
      ) : call ? (
        <Offer name={call} />
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
