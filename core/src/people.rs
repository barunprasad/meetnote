//! Who was in the meeting, from the calendar, and when that is enough to put a
//! name on the far end.
//!
//! Diarisation can only say "a second voice"; the calendar is what says whose.
//! The rule is deliberately narrow: a name goes on the far end only when there
//! is exactly one person it can be. A wrong name is worse than "Them" — it gets
//! quoted back as fact by anything reading the transcript over MCP.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Participant {
    /// Calendar invites from outside your organisation often carry no display
    /// name at all, only an address. Dropping those (as the first version did)
    /// left a one-on-one with an external candidate looking like nobody else
    /// was invited.
    pub name: Option<String>,
    pub email: Option<String>,
    /// The calendar account's own entry. The user is never the far end.
    pub is_me: bool,
    /// person | room | resource | group | unknown
    pub kind: String,
    pub declined: bool,
}

impl Participant {
    /// Someone who could be a voice on the far end of the call.
    fn could_be_far_end(&self) -> bool {
        !self.is_me && !self.declined && matches!(self.kind.as_str(), "person" | "unknown")
    }

    /// The best name available, spelled the way the meeting title spells it
    /// when the title mentions them — "Asha Verma" rather than "Averma".
    pub fn display_name(&self, title: &str) -> Option<String> {
        if let Some(name) = self.name.as_deref().map(str::trim) {
            if !name.is_empty() && !name.contains('@') {
                return Some(name.to_string());
            }
        }
        let local = self.email.as_deref()?.split('@').next()?.to_lowercase();
        let tokens: Vec<&str> = local
            .split(['.', '_', '-', '+'])
            .map(|t| t.trim_matches(|c: char| c.is_ascii_digit()))
            .filter(|t| !t.is_empty())
            .collect();
        if tokens.is_empty() {
            return None;
        }
        from_title(&tokens, title).or_else(|| Some(tokens.iter().map(|t| capitalise(t)).collect::<Vec<_>>().join(" ")))
    }
}

/// The one name to put on every far-end turn, if there is exactly one person
/// it can be.
///
/// - One invitee besides you: it is them. This holds even when diarisation
///   found several "voices" — on a single remote speaker it routinely splits
///   one person into two or three clusters, and that is the case the calendar
///   exists to correct.
/// - Several invitees: only when diarisation heard a single far-end voice and
///   exactly one invitee is named in the title ("Interview | Asha Verma |
///   …"), the usual shape of an interview where the recruiter who sent the
///   invite does not attend.
/// - Otherwise nobody is named; the user can name speakers by hand.
pub fn the_other_person(participants: &[Participant], title: &str, far_end_voices: usize) -> Option<String> {
    let far: Vec<&Participant> = participants.iter().filter(|p| p.could_be_far_end()).collect();
    if far.len() == 1 {
        return far[0].display_name(title);
    }
    if far.len() < 2 || far_end_voices != 1 {
        return None;
    }
    let title_lower = title.to_lowercase();
    let mut in_title = far
        .iter()
        .filter_map(|p| p.display_name(title))
        .filter(|name| title_lower.contains(&name.to_lowercase()));
    match (in_title.next(), in_title.next()) {
        (Some(name), None) => Some(name),
        _ => None,
    }
}

/// Finds the address's owner in the title: either every token of the address
/// as words of the title ("asha.verma"), or an initial plus surname
/// ("averma" → a word starting with a followed by "Verma").
fn from_title(tokens: &[&str], title: &str) -> Option<String> {
    let words: Vec<&str> = title
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .collect();
    let lower: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();

    // Every token appears, in order, as adjacent words.
    if let Some(start) = (0..lower.len()).find(|&i| {
        tokens.iter().enumerate().all(|(k, t)| lower.get(i + k).is_some_and(|w| w == t))
    }) {
        return Some(words[start..start + tokens.len()].join(" "));
    }

    // A single token of initial + surname.
    if let [only] = tokens {
        for i in 1..lower.len() {
            let surname = &lower[i];
            let first = &lower[i - 1];
            if surname.len() >= 3
                && only.len() > surname.len()
                && only.ends_with(surname.as_str())
                && only[..only.len() - surname.len()].starts_with(&first[..1])
            {
                return Some(format!("{} {}", words[i - 1], words[i]));
            }
        }
    }
    None
}

fn capitalise(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TITLE: &str = "Acme India | Round 5 - Virtual Interview | Asha Verma | Lead Engineer (Web) - PLATFORM ENGINEERING - E0100 | Pune";

    fn person(name: Option<&str>, email: Option<&str>) -> Participant {
        Participant {
            name: name.map(Into::into), email: email.map(Into::into),
            is_me: false, kind: "person".into(), declined: false,
        }
    }
    fn me() -> Participant {
        Participant { is_me: true, ..person(Some("Barun Prasad"), Some("barun@example.com")) }
    }

    #[test]
    fn you_are_never_the_other_person() {
        // The first version counted the user as an attendee, so a one-on-one
        // had "two" people and nobody was named.
        let asha = person(Some("Asha Verma"), None);
        assert_eq!(the_other_person(&[me(), asha], TITLE, 3), Some("Asha Verma".into()));
    }

    #[test]
    fn an_external_invitee_with_only_an_address_is_still_named() {
        let cases = [
            ("asha.verma@gmail.com", "Asha Verma"),
            ("averma@corp.example", "Asha Verma"),
            ("ASHA_VERMA91@example.com", "Asha Verma"),
        ];
        for (email, expected) in cases {
            let guest = person(None, Some(email));
            assert_eq!(the_other_person(&[me(), guest], TITLE, 1).as_deref(), Some(expected), "{email}");
        }
        // Not in the title: the address itself, tidied.
        let guest = person(None, Some("priya.sharma@example.com"));
        assert_eq!(the_other_person(&[me(), guest], "Weekly sync", 1).as_deref(), Some("Priya Sharma"));
    }

    #[test]
    fn rooms_and_people_who_declined_are_not_voices() {
        let room = Participant { kind: "room".into(), ..person(Some("Pune-4F-Everest"), None) };
        let declined = Participant { declined: true, ..person(Some("Asha"), None) };
        let asha = person(Some("Asha Verma"), None);
        assert_eq!(the_other_person(&[me(), room, declined, asha], TITLE, 2), Some("Asha Verma".into()));
    }

    #[test]
    fn several_invitees_need_one_voice_and_one_name_in_the_title() {
        let recruiter = person(Some("Riya Recruiter"), None);
        let asha = person(None, Some("asha.verma@gmail.com"));
        let people = [me(), recruiter, asha];
        assert_eq!(the_other_person(&people, TITLE, 1), Some("Asha Verma".into()));
        assert_eq!(the_other_person(&people, TITLE, 2), None, "two voices: could be either");
        assert_eq!(the_other_person(&people, "Panel sync", 1), None, "nobody in the title");
    }

    #[test]
    fn nobody_else_invited_names_nobody() {
        assert_eq!(the_other_person(&[me()], TITLE, 1), None);
        assert_eq!(the_other_person(&[], TITLE, 1), None);
    }
}
