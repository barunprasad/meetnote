import { useCallback, useEffect, useRef, useState, KeyboardEvent, ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openUrl, revealItemInDir } from "@tauri-apps/plugin-opener";
import Markdown from "react-markdown";
import {
  announce,
  Announcer,
  CallEvent,
  clock,
  duration,
  Fragment,
  FragmentLog,
  LiveRecording,
  longDay,
  Meeting,
  Meter,
  parseNote,
  spokenClock,
  statusLabel,
  Stream,
  timeOfDay,
  Turn,
  useNoteTaking,
  who,
} from "./shared";
import "./styles.css";

interface ModelStatus {
  ready: boolean;
  megabytes: number;
}
interface Progress {
  megabytes: number;
  total_megabytes: number;
  done: boolean;
  error: string | null;
}
interface Hit {
  meeting_id: string;
  title: string | null;
  started_at: number;
  snippet: string;
}
interface Permissions {
  microphone: string;
  calendar: string;
  system_audio: string;
}
interface SystemInfo {
  speech_model: string;
  speech_model_ready: boolean;
  llm_model: string;
  llm_available: boolean;
  llm_installed: string[];
  segmentation_origin: string;
  embedding_origin: string;
  data_dir: string;
}

type Tab = "note" | "transcript";
type View = "meeting" | "settings" | "search";

const named = (meeting: { title: string | null }) => meeting.title ?? "Untitled meeting";

const isShortcut = (event: globalThis.KeyboardEvent, key: string) =>
  event.metaKey && !event.shiftKey && !event.altKey && event.key.toLowerCase() === key;

// ── toolbar pieces ─────────────────────────────────────────────────────────

const SearchIcon = () => (
  <svg className="icon" viewBox="0 0 16 16" aria-hidden="true">
    <circle cx="7" cy="7" r="4.5" />
    <path d="M10.5 10.5 14 14" />
  </svg>
);

/**
 * The Note / Transcript switch, with the keyboard behaviour tabs are expected
 * to have: arrows move between them, and only the selected one is tabbable.
 */
function Segmented({
  tabs,
  selected,
  onSelect,
  panelId,
}: {
  tabs: { id: Tab; label: string }[];
  selected: Tab;
  onSelect: (id: Tab) => void;
  panelId: string;
}) {
  const refs = useRef<(HTMLButtonElement | null)[]>([]);

  const onKeyDown = (event: KeyboardEvent) => {
    const i = tabs.findIndex((t) => t.id === selected);
    const next =
      event.key === "ArrowRight" ? (i + 1) % tabs.length
      : event.key === "ArrowLeft" ? (i - 1 + tabs.length) % tabs.length
      : event.key === "Home" ? 0
      : event.key === "End" ? tabs.length - 1
      : null;
    if (next === null) return;
    event.preventDefault();
    onSelect(tabs[next].id);
    refs.current[next]?.focus();
  };

  return (
    <div className="segmented" role="tablist" aria-label="Meeting view" onKeyDown={onKeyDown}>
      {tabs.map((tab, i) => (
        <button
          key={tab.id}
          ref={(el) => {
            refs.current[i] = el;
          }}
          id={`tab-${tab.id}`}
          role="tab"
          aria-selected={tab.id === selected}
          aria-controls={panelId}
          tabIndex={tab.id === selected ? 0 : -1}
          onClick={() => onSelect(tab.id)}
        >
          {tab.label}
        </button>
      ))}
    </div>
  );
}

/**
 * A small menu behind a button. Destructive actions live here rather than on
 * the surface: they should take a deliberate move to reach, but not a dialog
 * to dismiss — the undo bar is what makes them safe.
 */
function Menu({ label, items }: { label: string; items: { label: string; onSelect: () => void; hint?: string }[] }) {
  const [open, setOpen] = useState(false);
  const box = useRef<HTMLDivElement>(null);
  const first = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!open) return;
    first.current?.focus();
    const onDown = (event: MouseEvent) => {
      if (!box.current?.contains(event.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [open]);

  return (
    <div className="menu" ref={box} onKeyDown={(event) => {
      if (event.key === "Escape") {
        setOpen(false);
        (box.current?.firstElementChild as HTMLElement | null)?.focus();
      }
    }}>
      <button
        className="button small menu-button"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={label}
        onClick={() => setOpen((o) => !o)}
      >
        <svg className="icon" viewBox="0 0 16 16" aria-hidden="true" style={{ fill: "currentColor", stroke: "none" }}>
          <circle cx="3" cy="8" r="1.5" /><circle cx="8" cy="8" r="1.5" /><circle cx="13" cy="8" r="1.5" />
        </svg>
      </button>
      {open && (
        <ul className="menu-list" role="menu">
          {items.map((item, i) => (
            <li key={item.label} role="none">
              <button
                ref={i === 0 ? first : undefined}
                role="menuitem"
                onClick={() => {
                  setOpen(false);
                  item.onSelect();
                }}
              >
                <span>{item.label}</span>
                {item.hint && <span className="menu-hint" aria-hidden="true">{item.hint}</span>}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/**
 * What stands in for a confirmation dialog: the meeting goes at once and can be
 * brought back for a few seconds. Nothing on disk is touched until the window
 * closes, so undo is real rather than a re-import.
 */
function UndoBar({ what, onUndo }: { what: string; onUndo: () => void }) {
  return (
    <div className="undo" role="status">
      <span>{what}</span>
      <button className="button small" onClick={onUndo} aria-keyshortcuts="Meta+Z">
        Undo <span className="menu-hint" aria-hidden="true">⌘Z</span>
      </button>
    </div>
  );
}

// ── live recording ─────────────────────────────────────────────────────────

function Recording({ meetingId, startedAtMs }: { meetingId: string; startedAtMs: number }) {
  const { elapsed, level, fragments, commit } = useNoteTaking(meetingId, startedAtMs);

  return (
    <div className="recording-page">
      <header className="live-head">
        <p className="live-state">
          <span className="rec-dot" aria-hidden="true" />
          Recording
        </p>
        <h1 className="live-clock tnum" aria-label={`Recording, ${spokenClock(elapsed)} elapsed`}>
          {clock(elapsed)}
        </h1>
        <div className="live-meters">
          <Meter label="You" dbfs={level.mic_dbfs} you />
          <Meter label="Them" dbfs={level.system_dbfs} />
        </div>
      </header>

      <FragmentLog fragments={fragments} elapsed={elapsed} commit={commit} autoFocus />

      <p className="hint">
        The floating panel is open over your call, and anything you type there appears here too.
        Stop with the button at the top right or ⌘R, and the note is written straight after.
      </p>
    </div>
  );
}

// ── the timeline ───────────────────────────────────────────────────────────

/**
 * Who spoke when, drawn from the transcript rather than from the audio: the
 * levels are never stored, but the turns are, and they carry the same shape.
 * Amber pins are the moments something was typed.
 */
function Timeline({
  turns,
  fragments,
  length,
}: {
  turns: Turn[] | null;
  fragments: Fragment[];
  length: number;
}) {
  if (length <= 0) return null;
  const pct = (secs: number) => `${Math.max(0, Math.min(100, (secs / length) * 100))}%`;
  const step = length > 2400 ? 600 : length > 600 ? 300 : 60;
  const ticks: number[] = [];
  for (let t = step; t < length; t += step) ticks.push(t);
  const lane = (mine: boolean) => (turns ?? []).filter((turn) => (turn.speaker === "ME") === mine);

  return (
    <section
      className="timeline"
      aria-label={`Timeline of the meeting, with ${fragments.length} notes you typed`}
    >
      <div className="tl-track tl-ruler" aria-hidden="true">
        {ticks.map((t) => (
          <span className="tl-tick tnum" key={t} style={{ left: pct(t) }}>
            {clock(t)}
          </span>
        ))}
      </div>
      <span className="tl-name you">You</span>
      <span className="tl-name them">Them</span>
      {(["you", "them"] as const).map((which) => (
        <div className={`tl-track tl-lane ${which}`} key={which} aria-hidden="true">
          {lane(which === "you").map((turn, i) => (
            <i
              className="tl-block"
              key={i}
              style={{
                left: pct(turn.start_secs),
                width: `${Math.max(0.15, ((turn.end_secs - turn.start_secs) / length) * 100)}%`,
              }}
            />
          ))}
        </div>
      ))}
      <div className="tl-track tl-pins" aria-hidden="true">
        {fragments.map((fragment, i) => (
          <span className="tl-pin" key={i} style={{ left: pct(fragment.at_secs) }}>
            <i />
          </span>
        ))}
      </div>
      {turns !== null && turns.length === 0 && (
        <p className="tl-empty">No transcript yet, so there is nothing to draw.</p>
      )}
    </section>
  );
}

// ── after the meeting ──────────────────────────────────────────────────────

function CaptureCheck({ streams }: { streams: Stream[] }) {
  const silent = (s: Stream) => s.peak_dbfs !== null && s.peak_dbfs < -80;
  if (streams.length === 0) return null;
  const theirsMissing = streams.some((s) => s.label === "system" && silent(s));

  return (
    <section className="capture" aria-labelledby="capture-heading">
      <h2 id="capture-heading" className="small-heading">
        What was recorded
      </h2>
      <ul className="capture-list">
        {streams.map((stream) => (
          <li key={stream.label} className={silent(stream) ? "bad" : "ok"}>
            <span className="capture-mark" aria-hidden="true">
              {silent(stream) ? "!" : "✓"}
            </span>
            <span>{stream.label === "mic" ? "Your microphone" : "Everyone else"}</span>
            <span className="capture-fact">
              {silent(stream)
                ? "Silent — nothing was captured"
                : `${duration(stream.duration_secs ?? 0)} of audio`}
            </span>
          </li>
        ))}
      </ul>

      {theirsMissing && (
        <div className="callout alarm" role="note">
          <p>
            <strong>The other side wasn’t recorded.</strong> Their audio is silent for the whole
            meeting, which is what happens when macOS hasn’t allowed meetnote to record this
            computer’s audio. Your microphone and notes are safe.
          </p>
          <button
            className="button"
            onClick={() =>
              openUrl("x-apple.systempreferences:com.apple.preference.security?Privacy_AudioCapture")
            }
          >
            Open Audio Recording settings
          </button>
        </div>
      )}
    </section>
  );
}

/**
 * A speaker's name, renamed in place. The calendar names the far end only when
 * there is exactly one person it can be; everything else is left for you, and
 * a name given here applies to every line that speaker said in this meeting.
 */
function SpeakerName({ turn, onRename }: { turn: Turn; onRename: (speaker: string, name: string) => void }) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState("");

  if (!editing) {
    return (
      <button
        className="turn-who turn-who-button"
        title="Rename this speaker"
        aria-label={`${who(turn)}, rename this speaker`}
        onClick={() => {
          setDraft(turn.speaker_name ?? "");
          setEditing(true);
        }}
      >
        {who(turn)}
      </button>
    );
  }

  const save = () => {
    setEditing(false);
    const name = draft.trim();
    if (name !== (turn.speaker_name ?? "")) onRename(turn.speaker, name);
  };
  return (
    <input
      className="turn-who turn-who-input"
      value={draft}
      autoFocus
      placeholder={who(turn)}
      aria-label={`Name for ${who(turn)}`}
      onChange={(event) => setDraft(event.target.value)}
      onBlur={save}
      onKeyDown={(event) => {
        if (event.key === "Enter") event.currentTarget.blur();
        if (event.key === "Escape") setEditing(false);
      }}
    />
  );
}

/** The transcript, grouped so a run of lines from one person reads as one block. */
function Transcript({
  turns,
  onRename,
}: {
  turns: Turn[] | null;
  onRename: (speaker: string, name: string) => void;
}) {
  if (turns === null) return <p className="quiet">Loading the transcript…</p>;
  if (turns.length === 0)
    return (
      <p className="quiet">
        Nothing was transcribed. If the meeting has finished processing, check what was recorded
        above.
      </p>
    );

  const blocks: Turn[][] = [];
  for (const turn of turns) {
    const last = blocks[blocks.length - 1];
    if (last && last[0].speaker === turn.speaker) last.push(turn);
    else blocks.push([turn]);
  }

  return (
    <div className="transcript">
      {blocks.map((block, i) => (
        <div className={`turn${block[0].speaker === "ME" ? " you" : ""}`} key={i}>
          <p className="turn-head">
            <SpeakerName turn={block[0]} onRename={onRename} />
            <time aria-label={spokenClock(block[0].start_secs)}>{clock(block[0].start_secs)}</time>
          </p>
          <p className="turn-said">{block.map((t) => t.text).join(" ")}</p>
        </div>
      ))}
    </div>
  );
}

/** The note: each section led by the fragment you typed, at the time you typed it. */
function Note({ body, fragments }: { body: string; fragments: Fragment[] }) {
  const { preamble, sections } = parseNote(body, fragments);

  if (sections.length === 0) {
    return (
      <div className="prose" style={{ marginTop: "1.25rem" }}>
        <Markdown>{body}</Markdown>
      </div>
    );
  }

  return (
    <div className="note">
      {preamble && (
        <div className="prose">
          <Markdown>{preamble}</Markdown>
        </div>
      )}
      {sections.map((section, i) => (
        <section className="note-section" key={i} aria-labelledby={`fragment-${i}`}>
          <time
            className="note-time"
            aria-label={section.at_secs === null ? undefined : `typed at ${spokenClock(section.at_secs)}`}
          >
            {section.at_secs === null ? "" : clock(section.at_secs)}
          </time>
          <div>
            <h2 id={`fragment-${i}`} className="fragment">
              {section.fragment}
            </h2>
            {section.unverified ? (
              <p className="unverified">
                Not found in the transcript. This is only what you typed, so check it before relying
                on it.
              </p>
            ) : (
              <div className="prose">
                <Markdown>{section.body}</Markdown>
              </div>
            )}
          </div>
        </section>
      ))}
    </div>
  );
}

/** The title, editable in place — most meetings never match a calendar event. */
function Title({ meeting, onRenamed }: { meeting: Meeting; onRenamed: () => void }) {
  const [draft, setDraft] = useState(meeting.title ?? "");

  useEffect(() => setDraft(meeting.title ?? ""), [meeting.id, meeting.title]);

  const save = async () => {
    const title = draft.trim();
    if (!title || title === meeting.title) return;
    await invoke("rename_meeting", { meetingId: meeting.id, title });
    announce(`Renamed to ${title}`);
    onRenamed();
  };

  return (
    <h1 className="title">
      <input
        className="title-input"
        value={draft}
        placeholder="Untitled meeting"
        aria-label="Meeting title, editable"
        onChange={(event) => setDraft(event.target.value)}
        onBlur={save}
        onKeyDown={(event) => {
          if (event.key === "Enter") event.currentTarget.blur();
          if (event.key === "Escape") {
            setDraft(meeting.title ?? "");
            event.currentTarget.blur();
          }
        }}
      />
    </h1>
  );
}

function MeetingView({
  meeting,
  tab,
  panelId,
  onRenamed,
  onDelete,
  onDeleteRecordings,
}: {
  meeting: Meeting;
  tab: Tab;
  panelId: string;
  onRenamed: () => void;
  onDelete: () => void;
  onDeleteRecordings: () => void;
}) {
  const [fragments, setFragments] = useState<Fragment[]>([]);
  const [streams, setStreams] = useState<Stream[]>([]);
  const [body, setBody] = useState<string | null>(null);
  const [turns, setTurns] = useState<Turn[] | null>(null);

  useEffect(() => {
    setTurns(null);
    invoke<Fragment[]>("fragments", { meetingId: meeting.id }).then(setFragments);
    invoke<Stream[]>("streams", { meetingId: meeting.id }).then(setStreams);
    invoke<string | null>("note", { meetingId: meeting.id }).then(setBody);
    invoke<Turn[]>("turns", { meetingId: meeting.id, from: 0, to: Number.MAX_VALUE }).then(setTurns);
  }, [meeting.id, meeting.status]);

  const length = meeting.ended_at ? meeting.ended_at - meeting.started_at : 0;
  const working = meeting.status === "transcribing" || meeting.status === "enhancing";

  const renameSpeaker = async (speaker: string, name: string) => {
    await invoke("rename_speaker", { meetingId: meeting.id, speaker, name });
    setTurns(await invoke<Turn[]>("turns", { meetingId: meeting.id, from: 0, to: Number.MAX_VALUE }));
    announce(name ? `Speaker renamed to ${name}` : "Speaker name cleared");
  };

  const transcribeAgain = async () => {
    try {
      await invoke("retranscribe", { meetingId: meeting.id });
      announce("Transcribing again");
    } catch (error) {
      announce(String(error));
    }
  };

  return (
    <article className="page">
      <header>
        <p className="meeting-when">
          {longDay(meeting.started_at)} at {timeOfDay(meeting.started_at)}
          {length > 0 && <>, {duration(length)}</>}
          {meeting.status === "done" && streams.length === 0 && <>, recording deleted</>}
        </p>
        <div className="title-row">
          <Title meeting={meeting} onRenamed={onRenamed} />
          <Menu
            label="Meeting actions"
            items={[
              ...(streams.length > 0 && !working && meeting.status !== "recording"
                ? [{ label: "Transcribe again", onSelect: transcribeAgain }]
                : []),
              ...(streams.length > 0
                ? [{ label: "Delete recording, keep note", onSelect: onDeleteRecordings }]
                : []),
              { label: "Delete meeting", onSelect: onDelete, hint: "⌫" },
            ]}
          />
        </div>
        {meeting.status !== "done" && (
          <p className={`status ${meeting.status}`}>
            {working && <span className="spinner" aria-hidden="true" />}
            {statusLabel[meeting.status]}
            {working && (
              <span className="status-note">
                runs on this computer and takes a few minutes. You can keep using meetnote.
              </span>
            )}
          </p>
        )}
      </header>

      <Timeline turns={turns} fragments={fragments} length={length} />

      {meeting.status !== "done" && <CaptureCheck streams={streams} />}

      <div id={panelId} role="tabpanel" aria-labelledby={`tab-${tab}`} tabIndex={0}>
        {tab === "note" &&
          (body ? (
            <Note body={body} fragments={fragments} />
          ) : (
            <>
              <p className="quiet" style={{ marginTop: "1.25rem" }}>
                {meeting.status === "done"
                  ? "No note was written, because nothing was typed during this meeting. The transcript is still here."
                  : "The note will appear here once it’s written."}
              </p>
              {fragments.length > 0 && (
                <section className="typed" aria-labelledby="typed-heading">
                  <h2 id="typed-heading" className="small-heading">
                    What you typed
                  </h2>
                  <ol className="log-list">
                    {fragments.map((fragment, i) => (
                      <li key={i}>
                        <time aria-label={spokenClock(fragment.at_secs)}>
                          {clock(fragment.at_secs)}
                        </time>
                        <span>{fragment.text}</span>
                      </li>
                    ))}
                  </ol>
                </section>
              )}
            </>
          ))}
        {tab === "transcript" && <Transcript turns={turns} onRename={renameSpeaker} />}
      </div>
    </article>
  );
}

// ── settings ───────────────────────────────────────────────────────────────

const PRIVACY = "x-apple.systempreferences:com.apple.preference.security?";

function Grant({
  label,
  state,
  help,
  pane,
}: {
  label: string;
  state: string;
  help: string;
  pane: string;
}) {
  const good = state === "granted";
  // Not yet asked or not yet known is a pending state, not a failure; only a
  // refusal argues for attention.
  const refused = state === "denied" || state === "restricted";
  const words: Record<string, string> = {
    granted: "Allowed",
    denied: "Not allowed",
    "not-asked": "Not asked yet",
    unknown: "Not known yet",
    restricted: "Restricted",
  };
  return (
    <li className={`grant ${good ? "ok" : refused ? "refused" : "pending"}`}>
      <span className="grant-mark" aria-hidden="true">
        {good ? "✓" : refused ? "!" : "–"}
      </span>
      <span className="grant-label">{label}</span>
      <span className="grant-state">{words[state] ?? state.replace("-", " ")}</span>
      {!good && (
        <>
          <p className="grant-help">{help}</p>
          {state === "not-asked" ? (
            <p className="grant-help">macOS will ask the first time it’s needed.</p>
          ) : (
            <button className="button small grant-open" onClick={() => openUrl(PRIVACY + pane)}>
              Open settings
            </button>
          )}
        </>
      )}
    </li>
  );
}

function Settings({ model, progress }: { model: ModelStatus | null; progress: Progress | null }) {
  const [permissions, setPermissions] = useState<Permissions | null>(null);
  const [info, setInfo] = useState<SystemInfo | null>(null);

  useEffect(() => {
    invoke<Permissions>("permissions").then(setPermissions);
    invoke<SystemInfo>("system_info").then(setInfo);
  }, [model?.ready]);

  return (
    <div className="page settings">
      <h1 className="page-title">Settings</h1>

      <section aria-labelledby="s-permissions">
        <h2 id="s-permissions">Permissions</h2>
        {permissions ? (
          <ul className="grants">
            <Grant
              label="Microphone"
              state={permissions.microphone}
              pane="Privacy_Microphone"
              help="Without it, your own voice isn’t recorded."
            />
            <Grant
              label="This computer’s audio"
              state={permissions.system_audio}
              pane="Privacy_AudioCapture"
              help="Without it, everyone else’s voice isn’t recorded. This one can only be confirmed by a recording that actually carried sound, so it stays unknown until you’ve recorded once."
            />
            <Grant
              label="Calendar"
              state={permissions.calendar}
              pane="Privacy_Calendars"
              help="Without it, meetings are untitled and attendee names aren’t filled in. Everything else works."
            />
          </ul>
        ) : (
          <p className="quiet">Checking…</p>
        )}
      </section>

      <section aria-labelledby="s-speech">
        <h2 id="s-speech">Transcription</h2>
        <dl className="facts">
          <div>
            <dt>Speech model</dt>
            <dd>
              <code>{info?.speech_model ?? "—"}</code>
            </dd>
          </div>
          <div>
            <dt>Downloaded</dt>
            <dd>
              {model?.ready ? `Yes, ${model.megabytes} MB` : "No"}
              {!model?.ready && !progress && (
                <button className="button small" onClick={() => invoke("download_model")}>
                  Download
                </button>
              )}
            </dd>
          </div>
          <div>
            <dt>Language</dt>
            <dd>Detected for each meeting</dd>
          </div>
        </dl>
        <p className="hint">
          Language is never forced. Pinning it to English makes Whisper translate rather than
          transcribe, which silently rewrites mixed-language speech instead of recording it.
        </p>
      </section>

      <section aria-labelledby="s-notes">
        <h2 id="s-notes">Note writing</h2>
        <dl className="facts">
          <div>
            <dt>Runs on</dt>
            <dd>{info?.llm_available ? "Ollama, on this computer" : "Ollama isn’t running"}</dd>
          </div>
          <div>
            <dt>Model</dt>
            <dd>
              <code>{info?.llm_model ?? "—"}</code>
            </dd>
          </div>
          {info && info.llm_installed.length > 1 && (
            <div>
              <dt>Also installed</dt>
              <dd>{info.llm_installed.filter((m) => m !== info.llm_model).join(", ")}</dd>
            </div>
          )}
        </dl>
      </section>

      <section aria-labelledby="s-speakers">
        <h2 id="s-speakers">Telling speakers apart</h2>
        <dl className="facts">
          <div>
            <dt>Segmentation model</dt>
            <dd>{info?.segmentation_origin ?? "—"}</dd>
          </div>
          <div>
            <dt>Voice model</dt>
            <dd>{info?.embedding_origin ?? "—"}</dd>
          </div>
        </dl>
        <p className="hint">
          Which words are yours is decided by which audio they arrived on — your microphone or the
          computer — never by these models. They only tell the other people apart.
        </p>
      </section>

      <section aria-labelledby="s-storage">
        <h2 id="s-storage">Storage</h2>
        <dl className="facts">
          <div>
            <dt>Folder</dt>
            <dd>
              <code className="path">{info?.data_dir ?? "—"}</code>
              {info && (
                <button className="button small" onClick={() => revealItemInDir(info.data_dir)}>
                  Show in Finder
                </button>
              )}
            </dd>
          </div>
        </dl>
        <p className="hint">
          Recordings, transcripts and notes stay on this computer. Audio takes about 350 MB an hour
          and nothing deletes it yet, so this folder grows until you clear it.
        </p>
      </section>
    </div>
  );
}

// ── search ─────────────────────────────────────────────────────────────────

function SearchResults({
  query,
  hits,
  onOpen,
}: {
  query: string;
  hits: Hit[];
  onOpen: (id: string) => void;
}) {
  return (
    <div className="page">
      <h1 className="page-title">
        {hits.length === 0 ? "No matches" : `${hits.length} ${hits.length === 1 ? "match" : "matches"}`} for
        “{query}”
      </h1>
      {hits.length === 0 && (
        <p className="quiet" style={{ marginTop: "0.75rem" }}>
          Search looks through written notes, so meetings still being processed won’t show up yet.
        </p>
      )}
      <ul className="hits">
        {hits.map((hit) => (
          <li key={hit.meeting_id}>
            <button className="hit" onClick={() => onOpen(hit.meeting_id)}>
              <span className="hit-title">{named(hit)}</span>
              <span className="hit-snippet">{hit.snippet}</span>
              <span className="hit-when">{longDay(hit.started_at)}</span>
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}

// ── sidebar ────────────────────────────────────────────────────────────────

function dayHeading(epoch: number) {
  const date = new Date(epoch * 1000);
  const today = new Date();
  const yesterday = new Date();
  yesterday.setDate(today.getDate() - 1);
  if (date.toDateString() === today.toDateString()) return "Today";
  if (date.toDateString() === yesterday.toDateString()) return "Yesterday";
  return longDay(epoch);
}

function MeetingList({
  meetings,
  selectedId,
  onOpen,
  onDelete,
}: {
  meetings: Meeting[];
  selectedId: string | null;
  onOpen: (m: Meeting) => void;
  onDelete: (m: Meeting) => void;
}) {
  if (meetings.length === 0) {
    return <p className="side-empty">Your meetings will be listed here.</p>;
  }

  const groups = new Map<string, Meeting[]>();
  for (const meeting of meetings) {
    const key = dayHeading(meeting.started_at);
    groups.set(key, [...(groups.get(key) ?? []), meeting]);
  }

  return (
    <>
      {[...groups].map(([heading, group]) => (
        <section key={heading} aria-label={heading}>
          <h2 className="day">{heading}</h2>
          <ul>
            {group.map((meeting) => {
              const working = meeting.status === "transcribing" || meeting.status === "enhancing";
              const failed = meeting.status === "failed";
              return (
                <li key={meeting.id}>
                  <button
                    className="meeting-item"
                    aria-current={meeting.id === selectedId ? "page" : undefined}
                    aria-keyshortcuts="Delete"
                    onClick={() => onOpen(meeting)}
                    onKeyDown={(event) => {
                      if (event.key !== "Backspace" && event.key !== "Delete") return;
                      event.preventDefault();
                      // Focus moves off the row that is about to vanish.
                      const row = event.currentTarget.closest("li");
                      const next = (row?.nextElementSibling ?? row?.previousElementSibling)
                        ?.querySelector("button") as HTMLElement | null;
                      onDelete(meeting);
                      next?.focus();
                    }}
                  >
                    <span className="item-title">{named(meeting)}</span>
                    <span className={`item-meta${working ? " working" : failed ? " failed" : ""}`}>
                      {meeting.status === "done"
                        ? clock((meeting.ended_at ?? meeting.started_at) - meeting.started_at)
                        : statusLabel[meeting.status]}
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
        </section>
      ))}
    </>
  );
}

// ── banners ────────────────────────────────────────────────────────────────

function ModelBanner({ progress }: { progress: Progress | null }) {
  const downloading = progress && !progress.done && !progress.error;
  const percent = progress
    ? Math.round((progress.megabytes / Math.max(1, progress.total_megabytes)) * 100)
    : 0;

  return (
    <section className="banner" aria-labelledby="model-banner">
      <div className="banner-text">
        <h2 id="model-banner">
          {progress?.error
            ? "The speech model didn’t download"
            : "Transcription needs a speech model"}
        </h2>
        <p>
          {downloading
            ? `Downloading — ${progress.megabytes} of ${progress.total_megabytes} MB.`
            : progress?.error
              ? progress.error
              : "Meetings still record without it. It’s a one-time 547 MB download and stays on this computer."}
        </p>
        {downloading && (
          <div
            className="progress"
            role="progressbar"
            aria-label="Speech model download"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={percent}
          >
            <i style={{ transform: `scaleX(${percent / 100})` }} />
          </div>
        )}
      </div>
      {!downloading && (
        <button className="button" onClick={() => invoke("download_model")}>
          {progress?.error ? "Try again" : "Download model"}
        </button>
      )}
    </section>
  );
}

function CallBanner({
  call,
  onRecord,
  onDismiss,
}: {
  call: CallEvent;
  onRecord: () => void;
  onDismiss: () => void;
}) {
  return (
    <section className="banner call" aria-labelledby="call-banner">
      <div className="banner-text">
        <h2 id="call-banner">{call.name} is using your microphone and speakers</h2>
        <p>Record this meeting? Nothing is recorded unless you choose to.</p>
      </div>
      <div className="banner-actions">
        <button className="button" onClick={onDismiss}>
          Not now
        </button>
        <button className="button record" onClick={onRecord}>
          <span className="rec-dot" aria-hidden="true" />
          Record
        </button>
      </div>
    </section>
  );
}

// ── shell ──────────────────────────────────────────────────────────────────

export default function App() {
  const [meetings, setMeetings] = useState<Meeting[]>([]);
  const [selected, setSelected] = useState<Meeting | null>(null);
  const [view, setView] = useState<View>("meeting");
  const [tab, setTab] = useState<Tab>("note");
  const [live, setLive] = useState<LiveRecording | null>(null);
  // A detected call is an offer, never an action: recording a private meeting
  // without being asked would be the wrong default.
  const [call, setCall] = useState<CallEvent | null>(null);
  // Nothing can be transcribed until the model is on disk, so this is stated
  // up front rather than discovered after a meeting.
  const [model, setModel] = useState<ModelStatus | null>(null);
  const [progress, setProgress] = useState<Progress | null>(null);
  const [query, setQuery] = useState("");
  const [hits, setHits] = useState<Hit[]>([]);
  const search = useRef<HTMLInputElement>(null);
  // A deletion waits here for its undo window. Nothing on disk is touched
  // until it is committed, so undo restores rather than re-imports.
  const [pending, setPending] = useState<{ meeting: Meeting; audioOnly: boolean } | null>(null);
  const held = useRef<{ meeting: Meeting; audioOnly: boolean; timer: number } | null>(null);
  // Last known status of each meeting, so a note finishing can be announced.
  const statuses = useRef(new Map<string, string>());

  const refresh = useCallback(async () => {
    const recent = await invoke<Meeting[]>("recent_meetings", { limit: 200 });
    for (const meeting of recent) {
      const before = statuses.current.get(meeting.id);
      if (before && before !== meeting.status) {
        if (meeting.status === "done") announce(`Note ready: ${named(meeting)}`);
        if (meeting.status === "failed") announce(`Processing failed: ${named(meeting)}`);
      }
      statuses.current.set(meeting.id, meeting.status);
    }
    setMeetings(recent);
    // A meeting on screen goes stale as transcription progresses behind it.
    setSelected((current) =>
      current ? recent.find((m) => m.id === current.id) ?? current : current,
    );
    return recent;
  }, []);

  useEffect(() => {
    refresh();
    // The tray can start and stop recording without either window asking, so
    // the shell reads the truth rather than tracking it.
    invoke<LiveRecording | null>("live_recording").then(setLive);
  }, [refresh]);

  useEffect(() => {
    const call = listen<CallEvent>("call:changed", (event) => {
      const started = event.payload.state === "started";
      setCall(started ? event.payload : null);
      if (started)
        announce(
          `${event.payload.name} is using your microphone and speakers. Record this meeting?`,
        );
    });
    const started = listen("recording:started", () => {
      invoke<LiveRecording | null>("live_recording").then(setLive);
      setView("meeting");
      setSelected(null);
      announce("Recording started");
    });
    const stopped = listen("recording:stopped", async () => {
      setLive(null);
      const recent = await refresh();
      setSelected(recent[0] ?? null);
      setView("meeting");
      announce("Recording stopped. Transcribing on this computer.");
    });
    return () => {
      call.then((off) => off());
      started.then((off) => off());
      stopped.then((off) => off());
    };
  }, [refresh]);

  useEffect(() => {
    invoke<ModelStatus>("model_status").then(setModel);
    let spoken = -1;
    const onProgress = listen<Progress>("model:progress", (event) => {
      const p = event.payload;
      setProgress(p);
      // Every quarter, not every megabyte — a live region that never stops
      // talking is worse than none.
      const quarter = Math.floor((p.megabytes / Math.max(1, p.total_megabytes)) * 4);
      if (!p.done && !p.error && quarter > spoken) {
        spoken = quarter;
        if (quarter > 0) announce(`Speech model ${quarter * 25}% downloaded`);
      }
      if (p.error) announce(`Speech model download failed: ${p.error}`);
      if (p.done) {
        announce("Speech model downloaded");
        invoke<ModelStatus>("model_status").then(setModel);
      }
    });
    // Transcription finishes on its own thread; refresh when it lands.
    const onMeeting = listen<string>("meeting:updated", () => refresh());
    return () => {
      onProgress.then((off) => off());
      onMeeting.then((off) => off());
    };
  }, [refresh]);

  // Debounced so every keystroke is not a query against the index.
  useEffect(() => {
    if (!query.trim()) {
      setHits([]);
      setView((v) => (v === "search" ? "meeting" : v));
      return;
    }
    const timer = setTimeout(() => {
      invoke<Hit[]>("search", { query })
        .then((found) => {
          setHits(found);
          announce(`${found.length || "No"} ${found.length === 1 ? "match" : "matches"}`);
        })
        .catch(() => setHits([]));
      setView("search");
    }, 200);
    return () => clearTimeout(timer);
  }, [query]);

  const open = (meeting: Meeting) => {
    setSelected(meeting);
    setTab("note");
    setView("meeting");
  };
  const openById = (id: string) => {
    const meeting = meetings.find((m) => m.id === id);
    if (meeting) open(meeting);
  };

  const start = async () => {
    const { meeting_id } = await invoke<{ meeting_id: string }>("start_recording");
    setSelected(null);
    setView("meeting");
    setCall(null);
    setLive({ meeting_id, started_at: Math.floor(Date.now() / 1000) });
    announce("Recording started");
  };

  const stop = async () => {
    await invoke("stop_recording");
    setLive(null);
    const recent = await refresh();
    setSelected(recent[0] ?? null);
    setView("meeting");
    announce("Recording stopped. Transcribing on this computer.");
  };

  /** Carries out whatever is waiting, and forgets it. */
  const commitDelete = useCallback(async () => {
    const waiting = held.current;
    if (!waiting) return;
    clearTimeout(waiting.timer);
    held.current = null;
    setPending(null);
    try {
      await invoke(waiting.audioOnly ? "delete_recordings" : "delete_meeting", {
        meetingId: waiting.meeting.id,
      });
    } catch (error) {
      announce(`Could not delete: ${error}`);
    }
    refresh();
  }, [refresh]);

  const queueDelete = useCallback(
    async (meeting: Meeting, audioOnly: boolean) => {
      // One at a time: a second delete commits the first rather than losing it.
      await commitDelete();
      const timer = window.setTimeout(() => commitDelete(), 10_000);
      held.current = { meeting, audioOnly, timer };
      setPending({ meeting, audioOnly });
      if (!audioOnly) {
        setSelected((current) => (current?.id === meeting.id ? null : current));
      }
      announce(
        audioOnly
          ? `Recording deleted for ${named(meeting)}. Undo with command Z.`
          : `Deleted ${named(meeting)}. Undo with command Z.`,
      );
    },
    [commitDelete],
  );

  const undoDelete = useCallback(() => {
    const waiting = held.current;
    if (!waiting) return;
    clearTimeout(waiting.timer);
    held.current = null;
    setPending(null);
    if (!waiting.audioOnly) setSelected(waiting.meeting);
    announce(`Restored ${named(waiting.meeting)}`);
  }, []);

  // A pending delete must not be lost by quitting: commit it on the way out.
  useEffect(() => {
    const flush = () => {
      if (held.current) commitDelete();
    };
    window.addEventListener("beforeunload", flush);
    return () => window.removeEventListener("beforeunload", flush);
  }, [commitDelete]);

  // Keyboard first: the actions you reach for mid-call must not need a mouse.
  const toggle = useRef(() => {});
  toggle.current = live ? stop : start;
  useEffect(() => {
    const onKey = (event: globalThis.KeyboardEvent) => {
      if (isShortcut(event, "r")) {
        event.preventDefault();
        toggle.current();
      } else if (isShortcut(event, "k") || isShortcut(event, "f")) {
        event.preventDefault();
        search.current?.focus();
        search.current?.select();
      } else if (isShortcut(event, ",")) {
        event.preventDefault();
        setView("settings");
      } else if (isShortcut(event, "z") && held.current) {
        event.preventDefault();
        undoDelete();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [undoDelete]);

  const showTabs = !live && view === "meeting" && selected !== null;
  // Hidden, not deleted: it comes straight back if undone.
  const listed = pending && !pending.audioOnly
    ? meetings.filter((m) => m.id !== pending.meeting.id)
    : meetings;

  let page: ReactNode;
  if (live) {
    page = <Recording meetingId={live.meeting_id} startedAtMs={live.started_at * 1000} />;
  } else if (view === "settings") {
    page = <Settings model={model} progress={progress} />;
  } else if (view === "search") {
    page = <SearchResults query={query} hits={hits} onOpen={openById} />;
  } else if (selected) {
    page = (
      <MeetingView
        meeting={selected}
        tab={tab}
        panelId="meeting-panel"
        onRenamed={refresh}
        onDelete={() => queueDelete(selected, false)}
        onDeleteRecordings={() => queueDelete(selected, true)}
      />
    );
  } else {
    page = (
      <div className="page welcome">
        <h1 className="page-title">Start recording before the meeting begins</h1>
        <p className="quiet">
          While it runs, type whatever matters in a few words. Afterwards meetnote transcribes the
          call and writes a note from what you typed — all on this computer.
        </p>
        <button className="button record" onClick={start} aria-keyshortcuts="Meta+R">
          <span className="rec-dot" aria-hidden="true" />
          Start recording
        </button>
      </div>
    );
  }

  return (
    <div className="shell">
      <nav className="sidebar" aria-label="Meetings">
        <div className="side-list">
          <MeetingList
            meetings={listed}
            selectedId={view === "meeting" && !live ? selected?.id ?? null : null}
            onOpen={open}
            onDelete={(meeting) => queueDelete(meeting, false)}
          />
        </div>
        <div className="side-foot">
          <button
            className="side-link"
            aria-current={view === "settings" ? "page" : undefined}
            onClick={() => setView("settings")}
            aria-keyshortcuts="Meta+,"
          >
            Settings
          </button>
          <span className="side-note">On this Mac</span>
        </div>
      </nav>

      <div className="main">
        <div className="toolbar">
          <form role="search" className="search" onSubmit={(event) => event.preventDefault()}>
            <SearchIcon />
            <input
              ref={search}
              type="search"
              value={query}
              placeholder="Search notes or jump to a meeting"
              aria-label="Search notes"
              aria-keyshortcuts="Meta+K"
              onChange={(event) => setQuery(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Escape") {
                  setQuery("");
                  event.currentTarget.blur();
                }
              }}
            />
            <span className="hint-key" aria-hidden="true">
              ⌘K
            </span>
          </form>

          <div className="toolbar-spacer" />

          {showTabs && (
            <Segmented
              tabs={[
                { id: "note", label: "Note" },
                { id: "transcript", label: "Transcript" },
              ]}
              selected={tab}
              onSelect={setTab}
              panelId="meeting-panel"
            />
          )}

          <button
            className={`button ${live ? "stop" : "record"}`}
            onClick={live ? stop : start}
            aria-keyshortcuts="Meta+R"
          >
            <span className={live ? "stop-square" : "rec-dot"} aria-hidden="true" />
            {live ? "Stop" : "Record"}
          </button>
        </div>

        <main className="main-scroll">
          {model && !model.ready && <ModelBanner progress={progress} />}
          {call && !live && (
            <CallBanner call={call} onRecord={start} onDismiss={() => setCall(null)} />
          )}
          {page}
        </main>

        {pending && (
          <UndoBar
            what={
              pending.audioOnly
                ? `Recording deleted for “${named(pending.meeting)}”`
                : `Deleted “${named(pending.meeting)}”`
            }
            onUndo={undoDelete}
          />
        )}
      </div>

      <Announcer />
    </div>
  );
}
