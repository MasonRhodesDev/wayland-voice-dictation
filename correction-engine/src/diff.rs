use crate::types::{
    CorrectionPair, InjectionContext, ReplacementEvent, TextChangeEvent, TextChangeOp,
};
use chrono::Utc;
use std::time::Duration;

const REPLACEMENT_GAP: Duration = Duration::from_secs(2);

pub fn group_replacement_events(events: &[TextChangeEvent]) -> Vec<ReplacementEvent> {
    let mut replacements = Vec::new();
    let mut i = 0;

    while i < events.len() {
        let event = &events[i];

        match event.operation {
            TextChangeOp::Delete => {
                if i + 1 < events.len() {
                    let next = &events[i + 1];
                    if next.operation == TextChangeOp::Insert
                        && next.start_pos == event.start_pos
                        && next.timestamp.duration_since(event.timestamp) <= REPLACEMENT_GAP
                    {
                        replacements.push(ReplacementEvent {
                            deleted_text: event.text.clone(),
                            inserted_text: next.text.clone(),
                            position: event.start_pos,
                            timestamp: next.timestamp,
                        });
                        i += 2;
                        continue;
                    }
                }

                replacements.push(ReplacementEvent {
                    deleted_text: event.text.clone(),
                    inserted_text: String::new(),
                    position: event.start_pos,
                    timestamp: event.timestamp,
                });
                i += 1;
            }
            TextChangeOp::Insert => {
                replacements.push(ReplacementEvent {
                    deleted_text: String::new(),
                    inserted_text: event.text.clone(),
                    position: event.start_pos,
                    timestamp: event.timestamp,
                });
                i += 1;
            }
        }
    }

    replacements
}

pub fn extract_corrections(
    context: &InjectionContext,
    events: &[TextChangeEvent],
) -> Vec<CorrectionPair> {
    if context.text.is_empty() || events.is_empty() {
        return Vec::new();
    }

    let injection_len = context.text.chars().count() as i32;
    let replacements = group_replacement_events(events);
    let mut corrections = Vec::new();

    for replacement in &replacements {
        if replacement.position < 0 || replacement.position > injection_len {
            continue;
        }

        if replacement.deleted_text.is_empty() && replacement.position == injection_len {
            continue;
        }

        if replacement.deleted_text.is_empty() && replacement.position == 0 {
            continue;
        }

        let (original, corrected) = if replacement.deleted_text.is_empty() {
            let (word_original, word_corrected) = extract_word_correction_for_insert(
                &context.text,
                replacement.position,
                &replacement.inserted_text,
            );
            (word_original, word_corrected)
        } else if replacement.inserted_text.is_empty() {
            (replacement.deleted_text.clone(), String::new())
        } else {
            (replacement.deleted_text.clone(), replacement.inserted_text.clone())
        };

        if original == corrected {
            continue;
        }

        let context_before = extract_context_before(&context.text, replacement.position, 20);
        let context_after = extract_context_after(
            &context.text,
            replacement.position + replacement.deleted_text.chars().count() as i32,
            20,
        );

        corrections.push(CorrectionPair {
            original,
            corrected,
            context_before,
            context_after,
            window_class: context.window_class.clone(),
            timestamp: Utc::now(),
        });
    }

    corrections
}

fn extract_word_correction_for_insert(
    injection_text: &str,
    insert_pos: i32,
    inserted_text: &str,
) -> (String, String) {
    let chars: Vec<char> = injection_text.chars().collect();
    let pos = insert_pos as usize;

    let word_start = chars[..pos.min(chars.len())]
        .iter()
        .rposition(|c| c.is_whitespace())
        .map(|p| p + 1)
        .unwrap_or(0);

    let word_end = chars[pos.min(chars.len())..]
        .iter()
        .position(|c| c.is_whitespace())
        .map(|p| p + pos)
        .unwrap_or(chars.len());

    let original_word: String = chars[word_start..word_end].iter().collect();

    let mut corrected_chars = chars[word_start..word_end].to_vec();
    let local_pos = pos - word_start;
    for (i, ch) in inserted_text.chars().enumerate() {
        corrected_chars.insert(local_pos + i, ch);
    }
    let corrected_word: String = corrected_chars.iter().collect();

    (original_word, corrected_word)
}

fn extract_context_before(text: &str, pos: i32, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let pos = (pos as usize).min(chars.len());
    let start = pos.saturating_sub(max_chars);
    chars[start..pos].iter().collect()
}

fn extract_context_after(text: &str, pos: i32, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let pos = (pos as usize).min(chars.len());
    let end = (pos + max_chars).min(chars.len());
    chars[pos..end].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn make_event(op: TextChangeOp, pos: i32, text: &str, offset: Duration) -> TextChangeEvent {
        let base = Instant::now();
        TextChangeEvent {
            operation: op,
            start_pos: pos,
            length: text.chars().count() as i32,
            text: text.to_string(),
            timestamp: base + offset,
            source_app: "test-app".to_string(),
        }
    }

    fn make_context(text: &str) -> InjectionContext {
        InjectionContext {
            text: text.to_string(),
            timestamp: Utc::now(),
            instant: Instant::now(),
            window_class: "test-app".to_string(),
            window_title: "Test Window".to_string(),
        }
    }

    #[test]
    fn test_group_delete_insert_pair() {
        let base = Instant::now();
        let events = vec![
            TextChangeEvent {
                operation: TextChangeOp::Delete,
                start_pos: 6,
                length: 5,
                text: "world".to_string(),
                timestamp: base,
                source_app: "test".to_string(),
            },
            TextChangeEvent {
                operation: TextChangeOp::Insert,
                start_pos: 6,
                length: 5,
                text: "earth".to_string(),
                timestamp: base + Duration::from_millis(100),
                source_app: "test".to_string(),
            },
        ];

        let groups = group_replacement_events(&events);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].deleted_text, "world");
        assert_eq!(groups[0].inserted_text, "earth");
        assert_eq!(groups[0].position, 6);
    }

    #[test]
    fn test_group_standalone_delete() {
        let events = vec![make_event(TextChangeOp::Delete, 6, "world", Duration::ZERO)];

        let groups = group_replacement_events(&events);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].deleted_text, "world");
        assert_eq!(groups[0].inserted_text, "");
    }

    #[test]
    fn test_group_standalone_insert() {
        let events = vec![make_event(TextChangeOp::Insert, 3, "l", Duration::ZERO)];

        let groups = group_replacement_events(&events);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].deleted_text, "");
        assert_eq!(groups[0].inserted_text, "l");
    }

    #[test]
    fn test_replacement_gap_timeout() {
        let base = Instant::now();
        let events = vec![
            TextChangeEvent {
                operation: TextChangeOp::Delete,
                start_pos: 6,
                length: 5,
                text: "world".to_string(),
                timestamp: base,
                source_app: "test".to_string(),
            },
            TextChangeEvent {
                operation: TextChangeOp::Insert,
                start_pos: 6,
                length: 5,
                text: "earth".to_string(),
                timestamp: base + Duration::from_secs(5),
                source_app: "test".to_string(),
            },
        ];

        let groups = group_replacement_events(&events);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].deleted_text, "world");
        assert_eq!(groups[0].inserted_text, "");
        assert_eq!(groups[1].deleted_text, "");
        assert_eq!(groups[1].inserted_text, "earth");
    }

    #[test]
    fn test_single_word_replacement() {
        let context = make_context("hello world");
        let base = Instant::now();
        let events = vec![
            TextChangeEvent {
                operation: TextChangeOp::Delete,
                start_pos: 6,
                length: 5,
                text: "world".to_string(),
                timestamp: base,
                source_app: "test-app".to_string(),
            },
            TextChangeEvent {
                operation: TextChangeOp::Insert,
                start_pos: 6,
                length: 5,
                text: "earth".to_string(),
                timestamp: base + Duration::from_millis(50),
                source_app: "test-app".to_string(),
            },
        ];

        let corrections = extract_corrections(&context, &events);
        assert_eq!(corrections.len(), 1);
        assert_eq!(corrections[0].original, "world");
        assert_eq!(corrections[0].corrected, "earth");
    }

    #[test]
    fn test_multi_word_replacement() {
        let context = make_context("the cash is here");
        let base = Instant::now();
        let events = vec![
            TextChangeEvent {
                operation: TextChangeOp::Delete,
                start_pos: 4,
                length: 4,
                text: "cash".to_string(),
                timestamp: base,
                source_app: "test-app".to_string(),
            },
            TextChangeEvent {
                operation: TextChangeOp::Insert,
                start_pos: 4,
                length: 5,
                text: "cache".to_string(),
                timestamp: base + Duration::from_millis(50),
                source_app: "test-app".to_string(),
            },
        ];

        let corrections = extract_corrections(&context, &events);
        assert_eq!(corrections.len(), 1);
        assert_eq!(corrections[0].original, "cash");
        assert_eq!(corrections[0].corrected, "cache");
    }

    #[test]
    fn test_no_correction_on_append() {
        let context = make_context("hello");
        let events = vec![make_event(TextChangeOp::Insert, 5, " world", Duration::ZERO)];

        let corrections = extract_corrections(&context, &events);
        assert!(corrections.is_empty(), "Appending text should not be a correction");
    }

    #[test]
    fn test_no_correction_on_prepend() {
        let context = make_context("world");
        let events = vec![make_event(TextChangeOp::Insert, 0, "hello ", Duration::ZERO)];

        let corrections = extract_corrections(&context, &events);
        assert!(corrections.is_empty(), "Prepending text should not be a correction");
    }

    #[test]
    fn test_delete_without_insert() {
        let context = make_context("hello world");
        let events = vec![make_event(TextChangeOp::Delete, 6, "world", Duration::ZERO)];

        let corrections = extract_corrections(&context, &events);
        assert_eq!(corrections.len(), 1);
        assert_eq!(corrections[0].original, "world");
        assert_eq!(corrections[0].corrected, "");
    }

    #[test]
    fn test_insert_within_injection() {
        let context = make_context("helo world");
        let events = vec![make_event(TextChangeOp::Insert, 3, "l", Duration::ZERO)];

        let corrections = extract_corrections(&context, &events);
        assert_eq!(corrections.len(), 1);
        assert_eq!(corrections[0].original, "helo");
        assert_eq!(corrections[0].corrected, "hello");
    }

    #[test]
    fn test_multiple_corrections() {
        let context = make_context("the cash is hear");
        let base = Instant::now();
        let events = vec![
            TextChangeEvent {
                operation: TextChangeOp::Delete,
                start_pos: 4,
                length: 4,
                text: "cash".to_string(),
                timestamp: base,
                source_app: "test-app".to_string(),
            },
            TextChangeEvent {
                operation: TextChangeOp::Insert,
                start_pos: 4,
                length: 5,
                text: "cache".to_string(),
                timestamp: base + Duration::from_millis(50),
                source_app: "test-app".to_string(),
            },
            TextChangeEvent {
                operation: TextChangeOp::Delete,
                start_pos: 13,
                length: 4,
                text: "hear".to_string(),
                timestamp: base + Duration::from_secs(3),
                source_app: "test-app".to_string(),
            },
            TextChangeEvent {
                operation: TextChangeOp::Insert,
                start_pos: 13,
                length: 4,
                text: "here".to_string(),
                timestamp: base + Duration::from_millis(3050),
                source_app: "test-app".to_string(),
            },
        ];

        let corrections = extract_corrections(&context, &events);
        assert_eq!(corrections.len(), 2);
        assert_eq!(corrections[0].original, "cash");
        assert_eq!(corrections[0].corrected, "cache");
        assert_eq!(corrections[1].original, "hear");
        assert_eq!(corrections[1].corrected, "here");
    }

    #[test]
    fn test_unicode_positions() {
        let context = make_context("hello 🌍 world");
        let base = Instant::now();
        let events = vec![
            TextChangeEvent {
                operation: TextChangeOp::Delete,
                start_pos: 8,
                length: 5,
                text: "world".to_string(),
                timestamp: base,
                source_app: "test-app".to_string(),
            },
            TextChangeEvent {
                operation: TextChangeOp::Insert,
                start_pos: 8,
                length: 5,
                text: "earth".to_string(),
                timestamp: base + Duration::from_millis(50),
                source_app: "test-app".to_string(),
            },
        ];

        let corrections = extract_corrections(&context, &events);
        assert_eq!(corrections.len(), 1);
        assert_eq!(corrections[0].original, "world");
        assert_eq!(corrections[0].corrected, "earth");
    }

    #[test]
    fn test_empty_injection() {
        let context = make_context("");
        let events = vec![make_event(TextChangeOp::Insert, 0, "hello", Duration::ZERO)];

        let corrections = extract_corrections(&context, &events);
        assert!(corrections.is_empty());
    }

    #[test]
    fn test_events_outside_injection_range() {
        let context = make_context("hello");
        let base = Instant::now();
        let events = vec![
            TextChangeEvent {
                operation: TextChangeOp::Delete,
                start_pos: 100,
                length: 3,
                text: "foo".to_string(),
                timestamp: base,
                source_app: "test-app".to_string(),
            },
            TextChangeEvent {
                operation: TextChangeOp::Insert,
                start_pos: 100,
                length: 3,
                text: "bar".to_string(),
                timestamp: base + Duration::from_millis(50),
                source_app: "test-app".to_string(),
            },
        ];

        let corrections = extract_corrections(&context, &events);
        assert!(corrections.is_empty());
    }

    #[test]
    fn test_no_op_correction_skipped() {
        let context = make_context("hello world");
        let base = Instant::now();
        let events = vec![
            TextChangeEvent {
                operation: TextChangeOp::Delete,
                start_pos: 6,
                length: 5,
                text: "world".to_string(),
                timestamp: base,
                source_app: "test-app".to_string(),
            },
            TextChangeEvent {
                operation: TextChangeOp::Insert,
                start_pos: 6,
                length: 5,
                text: "world".to_string(),
                timestamp: base + Duration::from_millis(50),
                source_app: "test-app".to_string(),
            },
        ];

        let corrections = extract_corrections(&context, &events);
        assert!(corrections.is_empty(), "No-op correction should be skipped");
    }

    #[test]
    fn test_context_before() {
        assert_eq!(extract_context_before("hello world foo", 6, 20), "hello ");
        assert_eq!(extract_context_before("hello", 0, 20), "");
        assert_eq!(extract_context_before("a very long prefix text here", 28, 10), " text here");
    }

    #[test]
    fn test_context_after() {
        assert_eq!(extract_context_after("hello world foo", 11, 20), " foo");
        assert_eq!(extract_context_after("hello", 5, 20), "");
        assert_eq!(extract_context_after("hello world", 0, 5), "hello");
    }
}
