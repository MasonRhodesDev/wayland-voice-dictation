use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

use crate::ctc_direct_engine::CtcDirectEngine;
use crate::ctc_engine::CtcEngine;
use crate::engine::TranscriptionEngine;
use crate::hotword_trie;
use crate::openai_engine::{OpenAiEngine, OpenAiOptions};
use crate::openai_realtime_engine::{self, OpenAiRealtimeEngine};
use crate::parakeet_engine::ParakeetEngine;
use crate::stream_engine::{LocalEngineDriver, LocalModel, StreamingEngine};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Parakeet,
    OpenAi,
}

impl Provider {
    pub fn key(self) -> &'static str {
        match self {
            Provider::Parakeet => "parakeet",
            Provider::OpenAi => "openai",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct EngineOptions {
    pub openai: OpenAiOptions,
}

#[derive(Debug, Clone)]
pub struct ModelSpec {
    pub provider: Provider,
    pub model_name: String,
}

impl ModelSpec {
    pub fn is_ctc(&self) -> bool {
        self.model_name.starts_with("ctc-") || self.model_name == "ctc"
    }

    pub fn is_ctc_direct(&self) -> bool {
        self.model_name == "ctc-direct"
    }
}

impl std::fmt::Display for ModelSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.provider.key(), self.model_name)
    }
}

impl ModelSpec {
    pub fn parse(spec: &str) -> Result<Self> {
        let parts: Vec<&str> = spec.splitn(2, ':').collect();
        if parts.len() != 2 {
            return Err(anyhow!(
                "Invalid model spec '{}', expected format 'provider:model_name'",
                spec
            ));
        }

        let provider = match parts[0] {
            "parakeet" => Provider::Parakeet,
            "openai" => Provider::OpenAi,
            other => {
                return Err(anyhow!(
                    "Unsupported engine '{}'. Supported: 'parakeet', 'openai'.",
                    other
                ))
            }
        };

        Ok(Self { provider, model_name: parts[1].to_string() })
    }

    fn get_models_dir() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".config").join("voice-dictation").join("models")
    }

    pub fn model_path(&self) -> PathBuf {
        if self.is_ctc() || self.is_ctc_direct() {
            Self::get_models_dir().join("parakeet-ctc")
        } else {
            Self::get_models_dir().join("parakeet")
        }
    }

    pub fn is_available(&self) -> bool {
        if self.provider == Provider::OpenAi {
            return std::env::var("OPENAI_API_KEY").map(|k| !k.is_empty()).unwrap_or(false);
        }
        let path = self.model_path();
        if self.is_ctc() || self.is_ctc_direct() {
            has_onnx_model(&path) && path.join("tokenizer.json").exists()
        } else {
            path.join("encoder-model.onnx").exists()
                && path.join("decoder_joint-model.onnx").exists()
        }
    }

    pub fn create_engine(&self, sample_rate: u32) -> Result<Arc<dyn TranscriptionEngine>> {
        if self.provider != Provider::Parakeet {
            return Err(anyhow!(
                "create_engine supports only 'parakeet'; use create_streaming_engine for '{}'",
                self
            ));
        }
        if self.is_ctc_direct() {
            info!(
                "Creating CTC Direct engine (ONNX + beam search) for model '{}'",
                self.model_name
            );
            let model_path = self.model_path();
            let engine = CtcDirectEngine::new(model_path, sample_rate)?;
            Ok(Arc::new(engine))
        } else if self.is_ctc() {
            info!("Creating parakeet CTC engine with model '{}'", self.model_name);
            let model_path = self.model_path();
            let hotwords_path = Some(hotword_trie::default_hotwords_path());
            let beam_width = 10;
            let engine = CtcEngine::new(model_path, sample_rate, hotwords_path, beam_width)?;
            Ok(Arc::new(engine))
        } else {
            info!("Creating parakeet TDT engine with model '{}'", self.model_name);
            let model_path = self.model_path();
            let engine = ParakeetEngine::new(model_path, sample_rate)?;
            Ok(Arc::new(engine))
        }
    }

    pub fn create_streaming_engine(
        &self,
        sample_rate: u32,
        options: &EngineOptions,
    ) -> Result<Arc<dyn StreamingEngine>> {
        match self.provider {
            Provider::OpenAi if openai_realtime_engine::is_realtime_model(&self.model_name) => {
                info!(
                    "Creating OpenAI realtime engine (model '{}', delay '{}', {} keywords)",
                    self.model_name,
                    options.openai.delay,
                    options.openai.keywords.len()
                );
                Ok(Arc::new(OpenAiRealtimeEngine::new(
                    self.model_name.clone(),
                    sample_rate,
                    options.openai.clone(),
                )?))
            }
            Provider::OpenAi => {
                info!(
                    "Creating OpenAI streaming engine (model '{}', {} keywords)",
                    self.model_name,
                    options.openai.keywords.len()
                );
                Ok(Arc::new(OpenAiEngine::new(
                    self.model_name.clone(),
                    sample_rate,
                    options.openai.clone(),
                )?))
            }
            Provider::Parakeet => {
                let model = self.build_local_model(sample_rate)?;
                Ok(Arc::new(LocalEngineDriver::new(model)))
            }
        }
    }

    fn build_local_model(&self, sample_rate: u32) -> Result<Arc<dyn LocalModel>> {
        let model_path = self.model_path();
        if self.is_ctc_direct() {
            info!(
                "Creating CTC Direct engine (ONNX + beam search) for model '{}'",
                self.model_name
            );
            Ok(Arc::new(CtcDirectEngine::new(model_path, sample_rate)?))
        } else if self.is_ctc() {
            info!("Creating parakeet CTC engine with model '{}'", self.model_name);
            let hotwords_path = Some(hotword_trie::default_hotwords_path());
            Ok(Arc::new(CtcEngine::new(model_path, sample_rate, hotwords_path, 10)?))
        } else {
            info!("Creating parakeet TDT engine with model '{}'", self.model_name);
            Ok(Arc::new(ParakeetEngine::new(model_path, sample_rate)?))
        }
    }
}

fn has_onnx_model(dir: &std::path::Path) -> bool {
    let candidates = ["model.onnx", "model_fp16.onnx", "model_int8.onnx", "model_q4.onnx"];
    for candidate in &candidates {
        if dir.join(candidate).exists() {
            return true;
        }
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|s| s.to_str()) == Some("onnx") {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_parakeet_spec() {
        let spec = ModelSpec::parse("parakeet:default").unwrap();
        assert_eq!(spec.model_name, "default");
        assert!(!spec.is_ctc());
    }

    #[test]
    fn test_parse_ctc_spec() {
        let spec = ModelSpec::parse("parakeet:ctc-1.1b").unwrap();
        assert_eq!(spec.model_name, "ctc-1.1b");
        assert!(spec.is_ctc());

        let spec = ModelSpec::parse("parakeet:ctc-0.6b").unwrap();
        assert!(spec.is_ctc());
    }

    #[test]
    fn test_parse_invalid_format() {
        assert!(ModelSpec::parse("invalid").is_err());
        assert!(ModelSpec::parse("vosk:model").is_err());
        assert!(ModelSpec::parse("whisper:model").is_err());
    }

    #[test]
    fn test_parse_openai_spec() {
        let spec = ModelSpec::parse("openai:gpt-4o-transcribe").unwrap();
        assert_eq!(spec.provider, Provider::OpenAi);
        assert_eq!(spec.model_name, "gpt-4o-transcribe");
        assert!(!spec.is_ctc());
        assert_eq!(format!("{}", spec), "openai:gpt-4o-transcribe");
    }

    #[test]
    fn test_parakeet_spec_has_parakeet_provider() {
        let spec = ModelSpec::parse("parakeet:default").unwrap();
        assert_eq!(spec.provider, Provider::Parakeet);
        let openai = ModelSpec::parse("openai:gpt-4o-transcribe").unwrap();
        assert!(openai.create_engine(16000).is_err());
    }

    #[test]
    fn test_display() {
        let spec = ModelSpec::parse("parakeet:default").unwrap();
        assert_eq!(format!("{}", spec), "parakeet:default");

        let spec = ModelSpec::parse("parakeet:ctc-1.1b").unwrap();
        assert_eq!(format!("{}", spec), "parakeet:ctc-1.1b");
    }

    #[test]
    fn test_model_path_tdt() {
        let spec = ModelSpec::parse("parakeet:default").unwrap();
        let path = spec.model_path();
        assert!(path.to_str().unwrap().ends_with("models/parakeet"));
    }

    #[test]
    fn test_model_path_ctc() {
        let spec = ModelSpec::parse("parakeet:ctc-1.1b").unwrap();
        let path = spec.model_path();
        assert!(path.to_str().unwrap().ends_with("models/parakeet-ctc"));
    }

    #[test]
    fn test_parse_ctc_direct_spec() {
        let spec = ModelSpec::parse("parakeet:ctc-direct").unwrap();
        assert_eq!(spec.model_name, "ctc-direct");
        assert!(spec.is_ctc_direct());
    }

    #[test]
    fn test_model_path_ctc_direct() {
        let spec = ModelSpec::parse("parakeet:ctc-direct").unwrap();
        let path = spec.model_path();
        assert!(path.to_str().unwrap().ends_with("models/parakeet-ctc"));
    }
}
