//! The prompt. This is the product.
//!
//! Ported from the spike, where it went through two revisions against real
//! model output. Both failures are recorded in the rules because both will
//! recur if the wording drifts: the first version quoted the transcript back
//! instead of writing, and adding an action-item list reintroduced a topic the
//! user had deliberately not noted.

use meetnote_core::{NoteFragment, Turn};

/// Bumped whenever the wording below changes, so notes written under an older
/// revision stay identifiable in the database.
pub const VERSION: i64 = 1;

pub const SYSTEM: &str = "\
You expand a person's own meeting notes into a complete note.

You are given a transcript of a meeting and the terse fragments that person \
typed while it was happening. Their fragments are an outline of what they \
considered worth recording. Your job is to expand each fragment into something \
complete and useful, drawing the substance from the transcript.

You are not writing a summary of the meeting. A summary covers what the meeting \
was about; you are covering what THEY thought was worth writing down. These are \
different documents, and producing the first one when asked for the second is \
the main way this task is failed.";

const RULES: &str = "\
1. Their fragments are the outline. Work through them in the order they were \
written, one section per fragment. Keep their ordering and their emphasis — a \
fragment they wrote three words about may deserve a short section, and one they \
wrote nothing about does not get a section at all.
2. Each fragment is stamped with the moment it was typed. Use that: the material \
for a fragment is the conversation around that point, not the whole meeting. \
They were reacting to something said just then.
3. Expand each fragment using the transcript as source material. Pull in the \
specifics that fragment was pointing at: the numbers, names, dates, decisions, \
objections and commitments that appear in the transcript around that topic.
4. Write the note in your own words, as settled statements of fact. Do not quote \
the transcript, do not copy sentences of speech across, and strip conversational \
filler and discourse markers. The reader wants the substance of what was \
established, not a trimmed-down replay of the dialogue. Never state the same \
fact twice.
5. Keep their vocabulary — if they used a shorthand, an abbreviation or an \
internal term, use it rather than translating it into neutral language. This is \
about their terminology, not a licence to copy speech out of the transcript.
6. The transcript is code-switched and may be partly in Devanagari, because it \
was transcribed faithfully rather than translated. Write the note in the \
language their own notes are written in — normally English. Translate or \
transliterate as needed, but keep names, product terms and any phrase whose \
exact wording carries weight. Never drop a point because it was said in another \
language, and never leave a sentence untranslated in the finished note.
7. Attribute decisions and commitments to whoever made them. Refer to the person \
whose notes these are in the first person; name other participants where the \
transcript names them, and otherwise describe them by role.
8. Do not add sections for topics they did not gesture at, no matter how much \
time the meeting spent on them. Silence in their notes is a signal, not a gap \
to fill.
9. If a fragment is not supported anywhere in the transcript, still give it its \
section, and write exactly `_(unverified — not discussed in the transcript)_` as \
the body. Do not quietly drop it and do not invent support for it.
10. Never invent a name, number, date, or commitment that is not in the \
transcript.

Output markdown. Give each section a `##` heading — the fragment itself, as they \
wrote it — followed by the expanded prose on its own line beneath. Start directly \
with the first heading: no preamble, no restatement of these instructions, no \
closing commentary.";

fn clock(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    format!("{:02}:{:02}", total / 60, total % 60)
}

/// Builds the user turn. Fragments carry their timestamps so the model can
/// anchor each one to the conversation around it — the cheapest quality win
/// available, and the reason `at_secs` is stored in the first place.
pub fn build(turns: &[Turn], fragments: &[NoteFragment]) -> String {
    let transcript = turns
        .iter()
        .map(|turn| format!("[{}] {}: {}", clock(turn.start_secs), turn.speaker, turn.text))
        .collect::<Vec<_>>()
        .join("\n");

    let notes = fragments
        .iter()
        .map(|fragment| format!("[{}] {}", clock(fragment.at_secs), fragment.text))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "## Transcript\n\n\
         Speaker labels: ME is the person whose notes these are. THEM is everyone else \
         on the call. Times are minutes into the meeting.\n\n\
         {transcript}\n\n\
         ## Their notes, typed live during the meeting\n\n\
         Each line is stamped with the moment it was typed.\n\n\
         {notes}\n\n\
         ## Instructions\n\n\
         {RULES}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Vec<Turn>, Vec<NoteFragment>) {
        (
            vec![Turn {
                start_secs: 134.0,
                end_secs: 140.0,
                speaker: "ME".into(),
                speaker_name: None,
                text: "The Postgres migration is about forty percent done.".into(),
            }],
            vec![NoteFragment { at_secs: 138.0, text: "postgres migration 40%".into() }],
        )
    }

    #[test]
    fn fragments_carry_the_moment_they_were_typed() {
        let (turns, fragments) = fixture();
        let prompt = build(&turns, &fragments);
        // 138s = 02:18, and the transcript line it refers to is at 02:14 — the
        // proximity is the whole point of including both.
        assert!(prompt.contains("[02:18] postgres migration 40%"));
        assert!(prompt.contains("[02:14] ME:"));
    }

    #[test]
    fn the_instructions_that_were_learned_the_hard_way_are_present() {
        let (turns, fragments) = fixture();
        let prompt = build(&turns, &fragments);
        // v1 replayed the dialogue instead of writing.
        assert!(prompt.contains("Do not quote"));
        // An action-item list reintroduced a topic the user had not noted.
        assert!(prompt.contains("Silence in their notes is a signal"));
        // Unsupported fragments must survive rather than be dropped.
        assert!(prompt.contains("_(unverified"));
    }
}
