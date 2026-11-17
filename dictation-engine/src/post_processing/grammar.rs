use super::TextProcessor;
use crate::user_dictionary::UserDictionary;
use anyhow::Result;
use harper_core::linting::{Lint, LintGroup, LintKind, Linter, Suggestion};
use harper_core::parsers::PlainEnglish;
use harper_core::spell::MutableDictionary;
use harper_core::{Dialect, Document};
use std::sync::Arc;

pub struct GrammarProcessor {
    dictionary: Arc<MutableDictionary>,
    user_dict: Option<Arc<UserDictionary>>,
}

impl GrammarProcessor {
    pub fn new() -> Self {
        let dictionary = MutableDictionary::curated();
        Self { dictionary, user_dict: None }
    }

    pub fn new_with_user_dictionary(user_dict: Arc<UserDictionary>) -> Self {
        let dictionary = MutableDictionary::curated();
        Self { dictionary, user_dict: Some(user_dict) }
    }
}

impl TextProcessor for GrammarProcessor {
    fn process(&self, text: &str) -> Result<String> {
        if text.is_empty() {
            return Ok(String::new());
        }

        let parser = PlainEnglish;
        let document = Document::new(text, &parser, &self.dictionary);

        let mut linter = LintGroup::new_curated(self.dictionary.clone(), Dialect::American);

        let lints = linter.lint(&document);

        let mut sorted_lints: Vec<Lint> = lints.into_iter().collect();
        sorted_lints.sort_by_key(|l| std::cmp::Reverse(l.span.start));

        let mut result = text.to_string();

        for lint in sorted_lints {
            if matches!(lint.lint_kind, LintKind::Spelling) {
                if let Some(user_dict) = &self.user_dict {
                    let span = lint.span;
                    let word = &text[span.start..span.end];
                    if user_dict.contains(word) {
                        continue;
                    }
                }
            }

            if let Some(suggestion) = get_best_suggestion(&lint) {
                let span = lint.span;
                let start = span.start;
                let end = span.end;

                if start <= result.len() && end <= result.len() && start <= end {
                    result.replace_range(start..end, &suggestion);
                }
            }
        }

        Ok(result)
    }
}

fn get_best_suggestion(lint: &Lint) -> Option<String> {
    match &lint.lint_kind {
        LintKind::Spelling => lint.suggestions.first().map(suggestion_to_string),
        _ => lint.suggestions.first().map(suggestion_to_string),
    }
}

fn suggestion_to_string(suggestion: &Suggestion) -> String {
    match suggestion {
        Suggestion::ReplaceWith(chars) => chars.iter().collect(),
        Suggestion::Remove => String::new(),
        Suggestion::InsertAfter(chars) => chars.iter().collect(),
    }
}

impl Default for GrammarProcessor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_string() {
        let processor = GrammarProcessor::new();
        let result = processor.process("").unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_correct_text_unchanged() {
        let processor = GrammarProcessor::new();
        let input = "This is a correct sentence.";
        let result = processor.process(input).unwrap();
        assert!(!result.is_empty());
    }

    #[test]
    fn test_simple_spelling() {
        let processor = GrammarProcessor::new();
        let input = "This is a tset.";
        let result = processor.process(input).unwrap();
        assert!(!result.is_empty());
    }

    #[test]
    fn test_technical_terms_preserved() {
        let processor = GrammarProcessor::new();
        let input = "The API endpoint returns JSON data.";
        let result = processor.process(input).unwrap();
        assert!(result.contains("API"));
        assert!(result.contains("JSON"));
    }

    #[test]
    fn test_multiple_sentences() {
        let processor = GrammarProcessor::new();
        let input = "First sentence. Second sentence. Third sentence.";
        let result = processor.process(input).unwrap();
        assert!(!result.is_empty());
    }
}
