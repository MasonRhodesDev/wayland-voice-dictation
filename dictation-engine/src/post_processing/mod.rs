mod acronym;
mod fuzzy_vocab;
mod grammar;
mod llm_correction;
mod punctuation;
mod sanitize;
pub mod stages;
mod word_substitution;

use crate::user_dictionary::UserDictionary;
use anyhow::Result;
use std::sync::Arc;

pub use acronym::AcronymProcessor;
pub use fuzzy_vocab::FuzzyVocabularyProcessor;
pub use grammar::GrammarProcessor;
pub use llm_correction::{LlmCorrectionConfig, LlmCorrectionProcessor};
pub use punctuation::PunctuationProcessor;
pub use sanitize::SanitizationProcessor;
pub use sanitize::SanitizationRules;
pub use stages::{PipelinePass, Stage, StageContext, StageSwitches, LOCAL_MODEL_STAGES};
pub use word_substitution::WordSubstitutionProcessor;

pub trait TextProcessor: Send + Sync {
    fn process(&self, text: &str) -> Result<String>;
}

pub struct Pipeline {
    processors: Vec<Box<dyn TextProcessor>>,
}

impl Pipeline {
    pub fn new() -> Self {
        Self { processors: Vec::new() }
    }

    pub fn add_processor(&mut self, processor: Box<dyn TextProcessor>) {
        self.processors.push(processor);
    }

    pub fn from_config(
        enable_acronyms: bool,
        enable_punctuation: bool,
        enable_grammar: bool,
    ) -> Self {
        Self::from_config_with_dict(
            enable_acronyms,
            enable_punctuation,
            enable_grammar,
            None,
            false,
            None,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_config_with_dict(
        enable_acronyms: bool,
        enable_punctuation: bool,
        enable_grammar: bool,
        user_dict: Option<Arc<UserDictionary>>,
        enable_word_substitution: bool,
        word_sub: Option<WordSubstitutionProcessor>,
        enable_fuzzy_vocab: bool,
    ) -> Self {
        let switches = StageSwitches {
            acronyms: enable_acronyms,
            punctuation: enable_punctuation,
            word_substitution: enable_word_substitution,
            fuzzy_vocab: enable_fuzzy_vocab,
            grammar: enable_grammar,
        };
        let stages = stages::resolve_stages(LOCAL_MODEL_STAGES, None, switches);
        let ctx = StageContext { user_dict, word_sub, llm: None };
        Self::from_stages(&stages, &ctx, PipelinePass::Final)
    }

    pub fn process(&self, text: &str) -> Result<String> {
        let mut result = text.to_string();

        for processor in &self.processors {
            result = processor.process(&result)?;
        }

        Ok(result)
    }

    pub fn is_empty(&self) -> bool {
        self.processors.is_empty()
    }

    pub fn len(&self) -> usize {
        self.processors.len()
    }
}

impl Default for Pipeline {
    fn default() -> Self {
        Self::new()
    }
}
