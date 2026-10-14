//! Per-app writing style: which helper stages run and how the final text is
//! shaped for the focused application's category.
//!
//! A style is applied in two places. [`StyleRules::restrict`] turns stages off
//! through the legacy switches, so it composes with `[pipeline]` overrides and
//! the `enable_*` flags. [`TrailingPeriodProcessor`] runs after the pipeline,
//! so it also applies to hosted engines that declare no stages.

use super::{StageSwitches, TextProcessor};
use anyhow::Result;

/// Named style presets. The string names are the config vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StylePreset {
    /// Full helper chain: sentence casing, grammar, trailing punctuation.
    Formal,
    /// Formal, minus the trailing period on a single sentence.
    Casual,
    /// No sentence casing and no grammar pass; text stays as spoken.
    Literal,
}

impl StylePreset {
    pub const NAMES: [&'static str; 3] = ["formal", "casual", "literal"];

    pub fn name(self) -> &'static str {
        match self {
            StylePreset::Formal => "formal",
            StylePreset::Casual => "casual",
            StylePreset::Literal => "literal",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "formal" => Some(StylePreset::Formal),
            "casual" => Some(StylePreset::Casual),
            "literal" => Some(StylePreset::Literal),
            _ => None,
        }
    }

    pub fn rules(self) -> StyleRules {
        match self {
            StylePreset::Formal => StyleRules::default(),
            StylePreset::Casual => {
                StyleRules { strip_single_sentence_period: true, ..StyleRules::default() }
            }
            StylePreset::Literal => {
                StyleRules { capitalize: false, grammar: false, ..StyleRules::default() }
            }
        }
    }
}

/// What a style changes. The default changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StyleRules {
    /// Run the `punctuation` stage (sentence casing and the pronoun "I").
    pub capitalize: bool,
    /// Run the `grammar` stage (Harper).
    pub grammar: bool,
    /// Drop the final period when the text is a single sentence.
    pub strip_single_sentence_period: bool,
}

impl Default for StyleRules {
    fn default() -> Self {
        Self { capitalize: true, grammar: true, strip_single_sentence_period: false }
    }
}

impl StyleRules {
    /// Narrow the stage switches. Never turns a stage back on.
    pub fn restrict(&self, switches: StageSwitches) -> StageSwitches {
        StageSwitches {
            punctuation: switches.punctuation && self.capitalize,
            grammar: switches.grammar && self.grammar,
            ..switches
        }
    }

    /// Post-pipeline processor for this style, if it needs one.
    pub fn processor(&self) -> Option<Box<dyn TextProcessor>> {
        self.strip_single_sentence_period
            .then(|| Box::new(TrailingPeriodProcessor) as Box<dyn TextProcessor>)
    }
}

/// Removes the final period from a single sentence ("sounds good." becomes
/// "sounds good"). Leaves `?`, `!`, ellipses, abbreviations such as "e.g."
/// and anything with more than one sentence untouched.
pub struct TrailingPeriodProcessor;

impl TextProcessor for TrailingPeriodProcessor {
    fn process(&self, text: &str) -> Result<String> {
        Ok(strip_single_sentence_period(text))
    }
}

fn strip_single_sentence_period(text: &str) -> String {
    let trimmed = text.trim_end();
    let Some(body) = trimmed.strip_suffix('.') else {
        return text.to_string();
    };
    if body.ends_with('.') {
        return text.to_string(); // ellipsis
    }
    let last_word = body.rsplit(char::is_whitespace).next().unwrap_or("");
    if last_word.is_empty() || last_word.contains('.') {
        return text.to_string(); // bare "." or an abbreviation like "e.g."
    }
    let mut prev_terminator = false;
    for c in body.chars() {
        if prev_terminator && c.is_whitespace() {
            return text.to_string(); // more than one sentence
        }
        prev_terminator = matches!(c, '.' | '!' | '?');
    }
    format!("{}{}", body, &text[trimmed.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(s: &str) -> String {
        TrailingPeriodProcessor.process(s).unwrap()
    }

    #[test]
    fn strips_period_from_single_sentence() {
        assert_eq!(strip("Sounds good."), "Sounds good");
        assert_eq!(strip("I'll be there at 5."), "I'll be there at 5");
        assert_eq!(strip("ok. "), "ok ");
    }

    #[test]
    fn keeps_other_terminators_and_multiple_sentences() {
        assert_eq!(strip("Are you coming?"), "Are you coming?");
        assert_eq!(strip("Nice!"), "Nice!");
        assert_eq!(strip("Wait..."), "Wait...");
        assert_eq!(strip("Sure. See you then."), "Sure. See you then.");
        assert_eq!(strip("Great! See you then."), "Great! See you then.");
        assert_eq!(strip("no period"), "no period");
        assert_eq!(strip(""), "");
        assert_eq!(strip("."), ".");
    }

    #[test]
    fn keeps_abbreviations_and_decimals() {
        assert_eq!(strip("Bring snacks, e.g."), "Bring snacks, e.g.");
        assert_eq!(strip("Moved to the U.S."), "Moved to the U.S.");
        assert_eq!(strip("Version 1.5 is out."), "Version 1.5 is out");
    }

    #[test]
    fn presets_parse_and_round_trip() {
        for name in StylePreset::NAMES {
            assert_eq!(StylePreset::parse(name).unwrap().name(), name);
        }
        assert_eq!(StylePreset::parse(" Casual "), Some(StylePreset::Casual));
        assert_eq!(StylePreset::parse("shouty"), None);
    }

    #[test]
    fn formal_restricts_nothing() {
        let rules = StylePreset::Formal.rules();
        assert_eq!(rules, StyleRules::default());
        assert!(rules.processor().is_none());
        let s = rules.restrict(StageSwitches::default());
        assert!(s.punctuation && s.grammar && s.acronyms && s.fuzzy_vocab);
    }

    #[test]
    fn literal_drops_casing_and_grammar_only() {
        let s = StylePreset::Literal.rules().restrict(StageSwitches::default());
        assert!(!s.punctuation);
        assert!(!s.grammar);
        assert!(s.acronyms && s.word_substitution && s.fuzzy_vocab);
    }

    #[test]
    fn restrict_never_reenables_a_stage() {
        let off = StageSwitches { grammar: false, ..StageSwitches::default() };
        assert!(!StylePreset::Formal.rules().restrict(off).grammar);
    }

    #[test]
    fn casual_adds_trailing_period_processor() {
        let p = StylePreset::Casual.rules().processor().unwrap();
        assert_eq!(p.process("Sounds good.").unwrap(), "Sounds good");
    }
}
