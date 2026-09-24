// Types, formatting and the note-taking machinery shared by the main window
// and the floating panel. Both write fragments to the same meeting, so the
// behaviour lives in one place rather than being written twice and drifting.

import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// ── types mirrored from meetnote-core ──────────────────────────────────────

export type Status = "recording" | "transcribing" | "enhancing" | "done" | "failed";

export interface Meeting {
  id: string;
  started_at: number;
  ended_at: number | null;
  title: string | null;
  status: Status;
}
export interface Stream {
  label: string;
  duration_secs: number | null;
  peak_dbfs: number | null;
}
export interface Fragment {
  at_secs: number;
  text: string;
}
export interface Turn {
  start_secs: number;
  end_secs: number;
  speaker: string;
  speaker_name: string | null;
  text: string;
}
export interface Level {
  t: number;
  mic_dbfs: number;
  system_dbfs: number;
}
export interface CallEvent {
  state: "started" | "ended";
  name: string;
  app: string;
}
export interface LiveRecording {
  meeting_id: string;
  started_at: number;
}

// ── formatting ─────────────────────────────────────────────────────────────

export const clock = (seconds: number) => {
  const s = Math.max(0, Math.floor(seconds));
  const h = Math.floor(s / 3600);
  const mm = String(Math.floor((s % 3600) / 60)).padStart(2, "0");
  const ss = String(s % 60).padStart(2, "0");
  return h ? `${h}:${mm}:${ss}` : `${mm}:${ss}`;
};

/** The same moment, said the way a screen reader should say it. */
export const spokenClock = (seconds: number) => {
  const s = Math.max(0, Math.floor(seconds));
  const m = Math.floor(s / 60);
  return m ? `${m} minute${m === 1 ? "" : "s"} ${s % 60} seconds` : `${s} seconds`;
};

export const day = (epoch: number) =>
  new Date(epoch * 1000).toLocaleDateString(undefined, { day: "numeric", month: "short" });

export const longDay = (epoch: number) =>
  new Date(epoch * 1000).toLocaleDateString(undefined, {
    weekday: "long",
    day: "numeric",
    month: "long",
  });

export const timeOfDay = (epoch: number) =>
  new Date(epoch * 1000).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });

/** "42 minutes", "1 hour 5 minutes" — lengths are read, not measured. */
export const duration = (seconds: number) => {
  const minutes = Math.round(seconds / 60);
  if (minutes < 1) return "under a minute";
  const h = Math.floor(minutes / 60);
  const m = minutes % 60;
  const hours = h ? `${h} hour${h === 1 ? "" : "s"}` : "";
  const mins = m ? `${m} minute${m === 1 ? "" : "s"}` : "";
  return [hours, mins].filter(Boolean).join(" ");
};

/** Status in the words a person uses, not the pipeline's stage names. */
export const statusLabel: Record<Status, string> = {
  recording: "Recording",
  transcribing: "Transcribing",
  enhancing: "Writing note",
  done: "Ready",
  failed: "Failed",
};

/**
 * Who said it, in the words the reader knows.
 *
 * `ME` and `THEM` come from which audio stream the speech was in, which is a
 * fact; a name attached to `THEM-2` is a guess that something else resolved.
 */
export const who = (turn: Turn) => {
  if (turn.speaker_name) return turn.speaker_name;
  if (turn.speaker === "ME") return "You";
  const numbered = turn.speaker.match(/^THEM-(\d+)$/);
  return numbered ? `Speaker ${numbered[1]}` : "Them";
};

// ── the written note ───────────────────────────────────────────────────────

export interface NoteSection {
  /** The fragment exactly as typed — the prompt makes it the `##` heading. */
  fragment: string;
  body: string;
  /** The model found nothing in the transcript to support this fragment. */
  unverified: boolean;
  at_secs: number | null;
}

const normalise = (text: string) => text.toLowerCase().replace(/\s+/g, " ").trim();

/**
 * Splits the note back into the fragments it grew from.
 *
 * The enhancement prompt gives every section a `##` heading that is the
 * fragment verbatim, so the pairing is recoverable from the markdown alone;
 * matching against stored fragments only adds the moment it was typed.
 */
export function parseNote(markdown: string, fragments: Fragment[]) {
  const sections: NoteSection[] = [];
  const preamble: string[] = [];
  let current: NoteSection | null = null;

  for (const line of markdown.split("\n")) {
    const heading = line.match(/^##\s+(.*?)\s*$/);
    if (heading) {
      current = { fragment: heading[1], body: "", unverified: false, at_secs: null };
      sections.push(current);
    } else if (current) {
      current.body += line + "\n";
    } else {
      preamble.push(line);
    }
  }

  for (const section of sections) {
    section.body = section.body.trim();
    section.unverified = /^_?\(unverified\b/i.test(section.body);
    const key = normalise(section.fragment);
    const typed =
      fragments.find((f) => normalise(f.text) === key) ??
      fragments.find((f) => normalise(f.text).startsWith(key) || key.startsWith(normalise(f.text)));
    section.at_secs = typed ? typed.at_secs : null;
  }

  return { preamble: preamble.join("\n").trim(), sections };
}

// ── announcements ──────────────────────────────────────────────────────────

/**
 * One polite live region for the whole window. Recording starting, a call
 * being detected, a note finishing — all of these change the screen without
 * the person having done anything, so a screen reader has to be told.
 */
let speak: ((text: string) => void) | null = null;

export const announce = (text: string) => speak?.(text);

export function Announcer() {
  const [text, setText] = useState("");
  useEffect(() => {
    speak = (next) => {
      // Cleared first so the same sentence twice is still read twice.
      setText("");
      requestAnimationFrame(() => setText(next));
    };
    return () => {
      speak = null;
    };
  }, []);
  return (
    <div className="visually-hidden" role="status" aria-live="polite">
      {text}
    </div>
  );
}

// ── meters ─────────────────────────────────────────────────────────────────

/** dBFS → 0–100. −60 is the floor; anything quieter reads as flat. */
export const level = (dbfs: number) => Math.max(0, Math.min(100, ((dbfs + 60) / 60) * 100));

const loudness = (dbfs: number) => (dbfs < -70 ? "silent" : dbfs < -40 ? "quiet" : "speaking");

/**
 * A live level, drawn as a bar and exposed as a meter. The spoken value is a
 * word rather than a number: "silent" is what you need to hear to know the
 * other side isn't being captured.
 */
export function Meter({ label, dbfs, you }: { label: string; dbfs: number; you?: boolean }) {
  return (
    <div className={`meter${you ? " you" : ""}`}>
      <span className="meter-label">{label}</span>
      <div
        className="meter-track"
        role="meter"
        aria-label={`${label} level`}
        aria-valuemin={-60}
        aria-valuemax={0}
        aria-valuenow={Math.round(Math.max(-60, Math.min(0, dbfs)))}
        aria-valuetext={loudness(dbfs)}
      >
        <i style={{ transform: `scaleX(${level(dbfs) / 100})` }} />
      </div>
    </div>
  );
}

// ── the note-taking surface, minus its chrome ──────────────────────────────

/**
 * Elapsed time, live levels, and the fragment list for a meeting in progress.
 *
 * Fragments arrive from two places — this window and the other one — so the
 * list is seeded from the store and then kept current by the backend event
 * rather than by local state alone.
 */
export function useNoteTaking(meetingId: string, startedAtMs: number) {
  const [elapsed, setElapsed] = useState((Date.now() - startedAtMs) / 1000);
  const [level, setLevel] = useState<Level>({ t: 0, mic_dbfs: -120, system_dbfs: -120 });
  const [fragments, setFragments] = useState<Fragment[]>([]);

  useEffect(() => {
    const tick = setInterval(() => setElapsed((Date.now() - startedAtMs) / 1000), 500);
    return () => clearInterval(tick);
  }, [startedAtMs]);

  useEffect(() => {
    invoke<Fragment[]>("fragments", { meetingId }).then(setFragments).catch(() => {});
  }, [meetingId]);

  useEffect(() => {
    const level = listen<Level>("capture:level", (event) => setLevel(event.payload));
    const added = listen<[string, number, string]>("fragment:added", (event) => {
      const [id, at_secs, text] = event.payload;
      if (id !== meetingId) return;
      setFragments((prior) =>
        // The window that typed it already added it optimistically.
        prior.some((f) => f.at_secs === at_secs && f.text === text)
          ? prior
          : [...prior, { at_secs, text }],
      );
    });
    return () => {
      level.then((off) => off());
      added.then((off) => off());
    };
  }, [meetingId]);

  const commit = async (draft: string) => {
    const text = draft.trim();
    if (!text) return;
    const at = elapsed;
    // Optimistic: the panel must never make you wait mid-meeting.
    setFragments((prior) => [...prior, { at_secs: at, text }]);
    await invoke("add_fragment", { meetingId, atSecs: at, text });
  };

  return { elapsed, level, fragments, commit };
}

/**
 * The fragment list and the line you type into, shared by the main window
 * and the floating panel so both behave identically.
 */
export function FragmentLog({
  fragments,
  elapsed,
  commit,
  autoFocus,
}: {
  fragments: Fragment[];
  elapsed: number;
  commit: (draft: string) => Promise<void>;
  autoFocus?: boolean;
}) {
  const [draft, setDraft] = useState("");
  const [list, setList] = useState<HTMLOListElement | null>(null);

  // The newest fragment has to stay in view, however short the window.
  useEffect(() => {
    list?.scrollTo({ top: list.scrollHeight });
  }, [list, fragments.length]);

  return (
    <div className="log">
      {fragments.length === 0 ? (
        <p className="log-empty">
          Type what matters as it happens. A few words is enough — the note is written from them
          afterwards.
        </p>
      ) : (
        <ol className="log-list" ref={setList} aria-label="Your notes so far">
          {fragments.map((fragment, i) => (
            <li key={i}>
              <time aria-label={spokenClock(fragment.at_secs)}>{clock(fragment.at_secs)}</time>
              <span>{fragment.text}</span>
            </li>
          ))}
        </ol>
      )}

      <form
        className="composer"
        onSubmit={(event) => {
          event.preventDefault();
          commit(draft);
          setDraft("");
        }}
      >
        <time className="composer-time" aria-hidden="true">
          {clock(elapsed)}
        </time>
        <input
          id="composer"
          value={draft}
          autoFocus={autoFocus}
          autoComplete="off"
          spellCheck
          placeholder="Add a note"
          aria-label="Add a note, stamped with the current time"
          aria-describedby="composer-hint"
          onChange={(event) => setDraft(event.target.value)}
        />
        <span id="composer-hint" className="composer-hint">
          Return to add
        </span>
      </form>
    </div>
  );
}
