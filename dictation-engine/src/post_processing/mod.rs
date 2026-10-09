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

/// Trait for text post-processors.
///
/// Processors transform transcribed text by applying corrections,
/// punctuation, capitalization, or other transformations.
pub trait TextProcessor: Send + Sync {
    /// Process the input text and return the transformed result.
    fn process(&self, text: &str) -> Result<String>;
}

/// Pipeline that orchestrates multiple text processors.
///
/// Processors are applied in sequence, with each processor
/// receiving the output of the previous one.
pub struct Pipeline {
    processors: Vec<Box<dyn TextProcessor>>,
}

impl Pipeline {
    /// Create an empty pipeline.
    pub fn new() -> Self {
        Self { processors: Vec::new() }
    }

    /// Add a processor to the pipeline.
    pub fn add_processor(&mut self, processor: Box<dyn TextProcessor>) {
        self.processors.push(processor);
    }

    /// Create a pipeline from configuration.
    ///
    /// Enables processors based on configuration flags.
    /// Processors are applied in order: acronyms → punctuation → word substitution → grammar.
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

    /// Create a pipeline from the legacy `enable_*` flags.
    ///
    /// Kept for callers that predate named stages. It resolves to the local
    /// model stage chain filtered by the flags, so the result is identical to
    /// the original hard-coded order: acronyms, punctuation, word
    /// substitution, fuzzy vocabulary, grammar.
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

    /// Process text through all processors in the pipeline.
    ///
    /// Returns the final processed result, or the original text
    /// if no processors are enabled.
    pub fn process(&self, text: &str) -> Result<String> {
        let mut result = text.to_string();

        for processor in &self.processors {
            result = processor.process(&result)?;
        }

        Ok(result)
    }

    /// Check if the pipeline has any processors.
    pub fn is_empty(&self) -> bool {
        self.processors.is_empty()
    }

    /// Number of processors in the pipeline.
    pub fn len(&self) -> usize {
        self.processors.len()
    }
}

impl Default for Pipeline {
    fn default() -> Self {
        Self::new()
    }
}
