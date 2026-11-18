use std::time::{Duration, Instant};

use correction_engine::diff::DictatedSpan;
use correction_engine::types::TextChangeOp;
use correction_engine::TextChangeEvent;

const SETTLE: Duration = Duration::from_secs(1);
const MAX_FIELD_LEN: usize = 256 * 1024;

pub(crate) struct EditTracker {
    injection: DictatedSpan,
    committed: String,
    pending: Option<(String, Instant)>,
    source_app: String,
}

struct WordEdit {
    start: usize,
    deleted: String,
    inserted: String,
}

impl EditTracker {
    pub(crate) fn locate(text: &str, injected: &str, source_app: String) -> Option<Self> {
        if injected.is_empty() || text.len() > MAX_FIELD_LEN {
            return None;
        }
        let start = text[..text.rfind(injected)?].chars().count();
        Some(Self {
            injection: DictatedSpan::new(start..start + injected.chars().count()),
            committed: text.to_string(),
            pending: None,
            source_app,
        })
    }

    pub(crate) fn is_tracking(&self) -> bool {
        self.injection.is_tracking()
    }

    pub(crate) fn observe(&mut self, text: &str, now: Instant) -> Vec<TextChangeEvent> {
        if text == self.committed || text.len() > MAX_FIELD_LEN {
            self.pending = None;
            return Vec::new();
        }
        match &self.pending {
            Some((pending, since)) if pending == text => {
                if now.duration_since(*since) < SETTLE {
                    return Vec::new();
                }
            }
            _ => {
                self.pending = Some((text.to_string(), now));
                return Vec::new();
            }
        }

        let edit = word_diff(&self.committed, text);
        self.committed = text.to_string();
        self.pending = None;
        edit.map_or_else(Vec::new, |edit| self.apply(edit, now))
    }

    fn apply(&mut self, edit: WordEdit, now: Instant) -> Vec<TextChangeEvent> {
        let Some(offset) = self.injection.edit(
            edit.start,
            edit.deleted.chars().count(),
            edit.inserted.chars().count(),
        ) else {
            return Vec::new();
        };
        if edit.deleted.split_whitespace().eq(edit.inserted.split_whitespace()) {
            return Vec::new();
        }
        let start_pos = offset as i32;
        [(TextChangeOp::Delete, edit.deleted), (TextChangeOp::Insert, edit.inserted)]
            .into_iter()
            .filter(|(_, text)| !text.is_empty())
            .map(|(operation, text)| TextChangeEvent {
                operation,
                start_pos,
                length: text.chars().count() as i32,
                text,
                timestamp: now,
                source_app: self.source_app.clone(),
            })
            .collect()
    }
}

fn word_diff(before: &str, after: &str) -> Option<WordEdit> {
    let before: Vec<char> = before.chars().collect();
    let after: Vec<char> = after.chars().collect();
    let mut prefix = before.iter().zip(&after).take_while(|(b, a)| b == a).count();
    if prefix == before.len() && prefix == after.len() {
        return None;
    }
    let mut suffix = before[prefix..]
        .iter()
        .rev()
        .zip(after[prefix..].iter().rev())
        .take_while(|(b, a)| b == a)
        .count();
    while !(at_word_boundary(&before, prefix) && at_word_boundary(&after, prefix)) {
        prefix -= 1;
    }
    while !(at_word_boundary(&before, before.len() - suffix)
        && at_word_boundary(&after, after.len() - suffix))
    {
        suffix -= 1;
    }
    let shared_symbol = |b: Option<&char>, a: Option<&char>| matches!((b, a), (Some(b), Some(a)) if b == a && !b.is_alphanumeric());
    while prefix + suffix < before.len().min(after.len())
        && shared_symbol(before.get(prefix), after.get(prefix))
    {
        prefix += 1;
    }
    while prefix + suffix < before.len().min(after.len())
        && shared_symbol(before.get(before.len() - suffix - 1), after.get(after.len() - suffix - 1))
    {
        suffix += 1;
    }
    Some(WordEdit {
        start: prefix,
        deleted: before[prefix..before.len() - suffix].iter().collect(),
        inserted: after[prefix..after.len() - suffix].iter().collect(),
    })
}

fn at_word_boundary(chars: &[char], index: usize) -> bool {
    index == 0
        || index == chars.len()
        || chars[index - 1].is_whitespace()
        || chars[index].is_whitespace()
}

#[cfg(test)]
mod tests {
    use super::*;
    use correction_engine::diff::extract_corrections;
    use correction_engine::InjectionContext;

    const APP: &str = "com.apple.TextEdit";

    fn context(text: &str) -> InjectionContext {
        InjectionContext {
            text: text.to_string(),
            timestamp: chrono::Utc::now(),
            instant: Instant::now(),
            window_class: APP.to_string(),
            window_title: String::new(),
        }
    }

    fn settled(tracker: &mut EditTracker, text: &str, start: Instant) -> Vec<TextChangeEvent> {
        assert!(tracker.observe(text, start).is_empty());
        tracker.observe(text, start + SETTLE)
    }

    fn pairs(injected: &str, events: &[TextChangeEvent]) -> Vec<(String, String)> {
        extract_corrections(&context(injected), events)
            .into_iter()
            .map(|pair| (pair.original, pair.corrected))
            .collect()
    }

    #[test]
    fn a_word_fixed_after_existing_text_becomes_a_correction() {
        let injected = "the word is here";
        let mut tracker =
            EditTracker::locate("Notes so far. the word is here", injected, APP.into()).unwrap();

        let events = settled(&mut tracker, "Notes so far. the world is here", Instant::now());

        assert!(events.iter().all(|e| e.start_pos == 4 && e.source_app == APP));
        assert_eq!(pairs(injected, &events), [("word".to_string(), "world".to_string())]);
    }

    #[test]
    fn transposed_letters_are_reported_as_whole_words() {
        let injected = "I saw teh cat";
        let mut tracker = EditTracker::locate(injected, injected, APP.into()).unwrap();

        let events = settled(&mut tracker, "I saw the cat", Instant::now());

        assert_eq!(pairs(injected, &events), [("teh".to_string(), "the".to_string())]);
    }

    #[test]
    fn nothing_is_reported_until_the_field_settles() {
        let injected = "hello wrld";
        let mut tracker = EditTracker::locate(injected, injected, APP.into()).unwrap();
        let start = Instant::now();

        assert!(tracker.observe("hello w", start).is_empty());
        assert!(tracker.observe("hello wor", start + SETTLE / 2).is_empty());
        assert!(tracker.observe("hello world", start + SETTLE).is_empty());
        let events = tracker.observe("hello world", start + SETTLE * 2);

        assert_eq!(pairs(injected, &events), [("wrld".to_string(), "world".to_string())]);
    }

    #[test]
    fn edits_before_the_injection_shift_it_without_being_reported() {
        let injected = "fix this wrd";
        let mut tracker = EditTracker::locate("Intro. fix this wrd", injected, APP.into()).unwrap();
        let start = Instant::now();

        assert!(settled(&mut tracker, "Longer intro. fix this wrd", start).is_empty());
        let events = settled(&mut tracker, "Longer intro. fix this word", start + SETTLE * 2);

        assert!(events.iter().all(|e| e.start_pos == 9));
        assert_eq!(pairs(injected, &events), [("wrd".to_string(), "word".to_string())]);
    }

    #[test]
    fn whitespace_only_changes_are_ignored() {
        let injected = "two words";
        let mut tracker = EditTracker::locate(injected, injected, APP.into()).unwrap();

        assert!(settled(&mut tracker, "two  words", Instant::now()).is_empty());
    }

    #[test]
    fn a_field_without_the_injected_text_is_not_tracked() {
        assert!(EditTracker::locate("something else", "hello", APP.into()).is_none());
        assert!(EditTracker::locate("anything", "", APP.into()).is_none());
    }

    #[test]
    fn punctuation_next_to_a_fixed_word_is_left_out() {
        let injected = "I saw (teh, cat";
        let mut tracker = EditTracker::locate(injected, injected, APP.into()).unwrap();

        let events = settled(&mut tracker, "I saw (the, cat", Instant::now());

        assert_eq!(pairs(injected, &events), [("teh".to_string(), "the".to_string())]);
    }

    #[test]
    fn sending_a_dictated_chat_message_records_nothing() {
        let injected = "see you at noon";
        let mut tracker = EditTracker::locate(injected, injected, APP.into()).unwrap();
        let start = Instant::now();

        assert!(settled(&mut tracker, "", start).is_empty());
        assert!(!tracker.is_tracking());
        assert!(settled(&mut tracker, "see yu at noon", start + SETTLE * 2).is_empty());
    }

    #[test]
    fn switching_to_another_document_stops_tracking() {
        let injected = "draft one";
        let mut tracker = EditTracker::locate("Title\ndraft one", injected, APP.into()).unwrap();
        let start = Instant::now();

        assert!(settled(&mut tracker, "Shopping list\neggs", start).is_empty());
        assert!(!tracker.is_tracking());
        assert!(settled(&mut tracker, "Shopping list\neggz", start + SETTLE * 2).is_empty());
    }

    #[test]
    fn text_typed_after_the_injection_is_not_a_correction() {
        let injected = "the first part";
        let mut tracker = EditTracker::locate(injected, injected, APP.into()).unwrap();
        let start = Instant::now();

        assert!(settled(&mut tracker, "the first part and more", start).is_empty());
        let events = settled(&mut tracker, "the frist part and more", start + SETTLE * 2);
        assert!(tracker.is_tracking());
        assert_eq!(pairs(injected, &events), [("first".to_string(), "frist".to_string())]);
    }

    #[test]
    fn oversized_fields_are_not_tracked() {
        let text = format!("{} hello", "x".repeat(MAX_FIELD_LEN));
        assert!(EditTracker::locate(&text, "hello", APP.into()).is_none());
    }
}
