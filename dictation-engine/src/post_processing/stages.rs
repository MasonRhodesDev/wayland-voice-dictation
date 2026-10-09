//! Named post-processing stages and per-engine pipeline resolution.
//!
//! Every text helper is a named [`Stage`]. Each transcription engine declares
//! the stages it needs through [`crate::stream_engine::StreamingEngine::default_stages`]:
//! the local Parakeet models declare the full helper chain, while hosted
//! engines that already produce punctuated, cased, vocabulary-aware text
//! declare none. The `[pipeline]` config section can override the list per
//! provider, so stages can be swapped without code changes.
//!
//! Resolution order for one session:
//! 1. The provider's `[pipeline]` entry, unless it is missing or `"default"`.
//! 2. Otherwise the stages the engine declares.
//! 3. The legacy `enable_*` flags then act as global off switches, so an
//!    existing config keeps its exact behavior.

use std::sync::Arc;

use tracing::{debug, warn};

use super::{
    AcronymProcessor, FuzzyVocabularyProcessor, GrammarProcessor, LlmCorrectionConfig,
    LlmCorrectionProcessor, Pipeline, PunctuationProcessor, WordSubstitutionProcessor,
};
use crate::user_dictionary::UserDictionary;

/// One post-processing step. The string names are the config vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    /// "a p i" to "API".
    Acronyms,
    /// Sentence casing and the pronoun "I".
    Punctuation,
    /// Exact spoken-phrase rewrites from substitutions.txt.
    WordSubstitution,
    /// Snap near-misses onto the user dictionary.
    FuzzyVocab,
    /// Harper grammar and spell checking.
    Grammar,
    /// LLM rewrite of the final text (self-corrections, glossary terms).
    LlmCorrection,
}

/// Canonical application order. A resolved stage list is always sorted into
/// this order, so config order cannot produce a different result.
const ORDER: [Stage; 6] = [
    Stage::Acronyms,
    Stage::Punctuation,
    Stage::WordSubstitution,
    Stage::FuzzyVocab,
    Stage::Grammar,
    Stage::LlmCorrection,
];

/// The helper chain the local models need (and had before stages existed).
pub const LOCAL_MODEL_STAGES: &[Stage] = &[
    Stage::Acronyms,
    Stage::Punctuation,
    Stage::WordSubstitution,
    Stage::FuzzyVocab,
    Stage::Grammar,
];

impl Stage {
    pub fn name(self) -> &'static str {
        match self {
            Stage::Acronyms => "acronyms",
            Stage::Punctuation => "punctuation",
            Stage::WordSubstitution => "word_substitution",
            Stage::FuzzyVocab => "fuzzy_vocab",
            Stage::Grammar => "grammar",
            Stage::LlmCorrection => "llm_correction",
        }
    }

    pub fn parse(name: &str) -> Option<Stage> {
        ORDER.iter().copied().find(|s| s.name() == name)
    }

    /// Whether the stage is cheap enough to run on every live preview update.
    /// Grammar and LLM correction run on the final text only.
    pub fn runs_in_preview(self) -> bool {
        !matches!(self, Stage::Grammar | Stage::LlmCorrection)
    }
}

/// Legacy global switches from `[daemon]`. A `false` removes the stage from
/// every engine's plan.
#[derive(Debug, Clone, Copy)]
pub struct StageSwitches {
    pub acronyms: bool,
    pub punctuation: bool,
    pub word_substitution: bool,
    pub fuzzy_vocab: bool,
    pub grammar: bool,
}

impl Default for StageSwitches {
    fn default() -> Self {
        Self {
            acronyms: true,
            punctuation: true,
            word_substitution: true,
            fuzzy_vocab: true,
            grammar: true,
        }
    }
}

impl StageSwitches {
    fn allows(&self, stage: Stage) -> bool {
        match stage {
            Stage::Acronyms => self.acronyms,
            Stage::Punctuation => self.punctuation,
            Stage::WordSubstitution => self.word_substitution,
            Stage::FuzzyVocab => self.fuzzy_vocab,
            Stage::Grammar => self.grammar,
            Stage::LlmCorrection => true, // opt-in by listing it; no legacy flag
        }
    }
}

/// Parse a comma-separated config stage list.
///
/// A missing value or `"default"` returns `None`, which means "use the
/// engine's declaration". `"none"` or an empty string returns an empty list.
/// Unknown names are logged and skipped, never fatal.
pub fn parse_stage_list(value: Option<&str>) -> Option<Vec<Stage>> {
    let raw = value?.trim();
    if raw.eq_ignore_ascii_case("default") {
        return None;
    }
    if raw.is_empty() || raw.eq_ignore_ascii_case("none") {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for name in raw.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        match Stage::parse(name) {
            Some(s) if !out.contains(&s) => out.push(s),
            Some(_) => {}
            None => warn!(
                "pipeline: unknown stage '{}' ignored (known: {})",
                name,
                ORDER.iter().map(|s| s.name()).collect::<Vec<_>>().join(", ")
            ),
        }
    }
    Some(out)
}

/// Resolve the final stage list for a session (see module docs).
pub fn resolve_stages(
    declared: &[Stage],
    config_override: Option<&str>,
    switches: StageSwitches,
) -> Vec<Stage> {
    let chosen = parse_stage_list(config_override).unwrap_or_else(|| declared.to_vec());
    ORDER.iter().copied().filter(|s| chosen.contains(s) && switches.allows(*s)).collect()
}

/// Render a stage list for logs.
pub fn describe(stages: &[Stage]) -> String {
    if stages.is_empty() {
        "(none)".to_string()
    } else {
        stages.iter().map(|s| s.name()).collect::<Vec<_>>().join(" > ")
    }
}

/// Shared resources a stage may need when it is built.
#[derive(Clone, Default)]
pub struct StageContext {
    pub user_dict: Option<Arc<UserDictionary>>,
    pub word_sub: Option<WordSubstitutionProcessor>,
    pub llm: Option<LlmCorrectionConfig>,
}

/// Which pass a pipeline serves. Preview drops the slow final-only stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelinePass {
    Preview,
    Final,
}

impl Pipeline {
    /// Build a pipeline from a resolved stage list. A stage whose resource is
    /// missing (no substitutions file, no dictionary, no LLM config) is
    /// skipped with a warning instead of failing the session.
    pub fn from_stages(stages: &[Stage], ctx: &StageContext, pass: PipelinePass) -> Self {
        let mut pipeline = Pipeline::new();
        for &stage in stages {
            if pass == PipelinePass::Preview && !stage.runs_in_preview() {
                continue;
            }
            match stage {
                Stage::Acronyms => pipeline.add_processor(Box::new(AcronymProcessor::new())),
                Stage::Punctuation => {
                    pipeline.add_processor(Box::new(PunctuationProcessor::new()))
                }
                Stage::WordSubstitution => match &ctx.word_sub {
                    Some(ws) => pipeline.add_processor(Box::new(ws.clone())),
                    None => debug!("pipeline: word_substitution listed but no rules loaded"),
                },
                Stage::FuzzyVocab => match &ctx.user_dict {
                    Some(d) => pipeline
                        .add_processor(Box::new(FuzzyVocabularyProcessor::new(Arc::clone(d)))),
                    None => debug!("pipeline: fuzzy_vocab listed but no user dictionary"),
                },
                Stage::Grammar => match &ctx.user_dict {
                    Some(d) => pipeline.add_processor(Box::new(
                        GrammarProcessor::new_with_user_dictionary(Arc::clone(d)),
                    )),
                    None => pipeline.add_processor(Box::new(GrammarProcessor::new())),
                },
                Stage::LlmCorrection => match &ctx.llm {
                    Some(cfg) if cfg.is_configured() => pipeline.add_processor(Box::new(
                        LlmCorrectionProcessor::new(cfg.clone(), ctx.user_dict.clone()),
                    )),
                    _ => warn!(
                        "pipeline: llm_correction listed but [llm_correction] model is not set; skipping"
                    ),
                },
            }
        }
        pipeline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_keyword_uses_engine_declaration() {
        let got = resolve_stages(LOCAL_MODEL_STAGES, Some("default"), StageSwitches::default());
        assert_eq!(got, LOCAL_MODEL_STAGES);
        let got = resolve_stages(LOCAL_MODEL_STAGES, None, StageSwitches::default());
        assert_eq!(got, LOCAL_MODEL_STAGES);
    }

    #[test]
    fn hosted_engine_declares_nothing() {
        assert!(resolve_stages(&[], None, StageSwitches::default()).is_empty());
    }

    #[test]
    fn override_replaces_declaration_and_is_reordered() {
        let got = resolve_stages(
            LOCAL_MODEL_STAGES,
            Some("llm_correction, acronyms"),
            StageSwitches::default(),
        );
        assert_eq!(got, vec![Stage::Acronyms, Stage::LlmCorrection]);
    }

    #[test]
    fn none_and_empty_mean_no_stages() {
        for v in ["none", "", "  "] {
            assert!(
                resolve_stages(LOCAL_MODEL_STAGES, Some(v), StageSwitches::default()).is_empty()
            );
        }
    }

    #[test]
    fn legacy_switch_removes_stage_everywhere() {
        let switches = StageSwitches { grammar: false, ..StageSwitches::default() };
        let got = resolve_stages(LOCAL_MODEL_STAGES, None, switches);
        assert!(!got.contains(&Stage::Grammar));
        assert_eq!(got.len(), LOCAL_MODEL_STAGES.len() - 1);
    }

    #[test]
    fn unknown_names_are_skipped() {
        let got = resolve_stages(&[], Some("grammar,bogus"), StageSwitches::default());
        assert_eq!(got, vec![Stage::Grammar]);
    }

    #[test]
    fn preview_pass_drops_final_only_stages() {
        let ctx = StageContext::default();
        let p =
            Pipeline::from_stages(&[Stage::Acronyms, Stage::Grammar], &ctx, PipelinePass::Preview);
        assert_eq!(p.len(), 1);
        let p =
            Pipeline::from_stages(&[Stage::Acronyms, Stage::Grammar], &ctx, PipelinePass::Final);
        assert_eq!(p.len(), 2);
    }
}
