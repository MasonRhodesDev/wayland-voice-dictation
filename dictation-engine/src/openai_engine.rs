//! Hosted OpenAI transcription engine (batch).
//!
//! Opt-in alternative to the local Parakeet engines, selected via the config
//! model spec `openai:<model>` (e.g. `openai:gpt-transcribe`). Realtime models
//! (`openai:gpt-live-transcribe`) use [`crate::openai_realtime_engine`] instead. Implements the
//! push-based [`StreamingEngine`] contract directly:
//!
//! - `process_audio` buffers samples (like the local path).
//! - No `Partial` events — batch transcription can't stream, so the preview
//!   overlay stays quiet while speaking.
//! - `finish()` uploads the full buffer as a WAV to the OpenAI transcription
//!   endpoint on the tokio runtime and emits a single `Final` (or `Error`).
//!
//! The API key is read from `OPENAI_API_KEY` at construction; if it is absent
//! the engine fails to construct and the selector stays on the default engine.
//! There is **no** cross-engine fallback: a request failure surfaces as
//! `TranscriptEvent::Error`.

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, error};

use crate::post_processing::Stage;
use crate::stream_engine::{EngineError, EventStream, StreamingEngine, TranscriptEvent};

const TRANSCRIPTIONS_URL: &str = "https://api.openai.com/v1/audio/transcriptions";

/// Most keywords sent with one request. The API documents no hard cap, and
/// long lists dilute the hint, so keep it bounded.
const MAX_KEYWORDS: usize = 100;

/// `[openai]` config section. Strings are comma-separated lists so the
/// schema-driven config TUI can edit and preserve them.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenAiConfig {
    /// Free-text description of what you usually dictate.
    #[serde(default)]
    pub prompt: String,
    /// Send user-dictionary words as `keywords` hints.
    #[serde(default = "default_true")]
    pub keywords_from_dictionary: bool,
    /// Extra keyword hints, comma-separated.
    #[serde(default)]
    pub extra_keywords: String,
    /// Expected input languages, comma-separated ISO codes (e.g. "en").
    #[serde(default)]
    pub languages: String,
    /// Realtime latency/accuracy trade-off: minimal, low, medium, high, xhigh.
    #[serde(default = "default_delay")]
    pub delay: String,
    /// Realtime transcription WebSocket endpoint.
    #[serde(default = "default_realtime_url")]
    pub realtime_url: String,
}

fn default_true() -> bool {
    true
}
fn default_delay() -> String {
    "low".to_string()
}
pub fn default_realtime_url() -> String {
    "wss://api.openai.com/v1/realtime?intent=transcription".to_string()
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            keywords_from_dictionary: true,
            extra_keywords: String::new(),
            languages: String::new(),
            delay: default_delay(),
            realtime_url: default_realtime_url(),
        }
    }
}

/// Resolved request context shared by the batch and realtime engines.
#[derive(Debug, Clone, Default)]
pub struct OpenAiOptions {
    pub prompt: Option<String>,
    pub keywords: Vec<String>,
    pub languages: Vec<String>,
    pub delay: String,
    pub realtime_url: String,
    /// Batch endpoint override (tests, proxies). `None` uses the public API.
    pub transcriptions_url: Option<String>,
}

impl OpenAiConfig {
    /// Build request options, folding in user-dictionary words when enabled.
    pub fn to_options(&self, dictionary_words: &[String]) -> OpenAiOptions {
        let mut keywords: Vec<String> = Vec::new();
        let mut push = |w: &str| {
            let w = w.trim();
            let valid = w.chars().count() >= 3
                && !w.contains(['<', '>', '\r', '\n'])
                && !keywords.iter().any(|k| k.eq_ignore_ascii_case(w));
            if valid && keywords.len() < MAX_KEYWORDS {
                keywords.push(w.to_string());
            }
        };
        for w in self.extra_keywords.split(',') {
            push(w);
        }
        if self.keywords_from_dictionary {
            for w in dictionary_words {
                push(w);
            }
        }
        let languages = self
            .languages
            .split(',')
            .map(|l| l.trim().to_lowercase())
            .filter(|l| !l.is_empty())
            .collect();
        let delay = match self.delay.trim() {
            d @ ("minimal" | "low" | "medium" | "high" | "xhigh") => d.to_string(),
            _ => default_delay(),
        };
        let prompt = Some(self.prompt.trim().to_string()).filter(|p| !p.is_empty());
        let realtime_url = Some(self.realtime_url.trim().to_string())
            .filter(|u| !u.is_empty())
            .unwrap_or_else(default_realtime_url);
        OpenAiOptions { prompt, keywords, languages, delay, realtime_url, transcriptions_url: None }
    }
}

/// Models that accept `keywords` and `languages` (the 2026 transcription line).
pub fn supports_context_lists(model: &str) -> bool {
    model.starts_with("gpt-transcribe") || model.starts_with("gpt-live-transcribe")
}

/// Models that accept a free-text `prompt`.
pub fn supports_prompt(model: &str) -> bool {
    !model.contains("diarize")
}

pub struct OpenAiEngine {
    api_key: String,
    model: String,
    sample_rate: u32,
    options: OpenAiOptions,
    buffer: Arc<Mutex<Vec<i16>>>,
    event_tx: Arc<Mutex<Option<mpsc::UnboundedSender<TranscriptEvent>>>>,
    client: reqwest::Client,
}

impl OpenAiEngine {
    /// Construct the engine. Fails if `OPENAI_API_KEY` is unset/empty so the
    /// selector can fall back to the default engine and log the reason.
    pub fn new(model: String, sample_rate: u32, options: OpenAiOptions) -> Result<Self> {
        let api_key = api_key_from_env()?;

        Ok(Self {
            api_key,
            model,
            sample_rate,
            options,
            buffer: Arc::new(Mutex::new(Vec::new())),
            event_tx: Arc::new(Mutex::new(None)),
            client: reqwest::Client::new(),
        })
    }
}

impl StreamingEngine for OpenAiEngine {
    fn process_audio(&self, samples: &[i16]) -> Result<()> {
        self.buffer
            .lock()
            .map_err(|e| anyhow::anyhow!("audio buffer lock poisoned: {e}"))?
            .extend_from_slice(samples);
        Ok(())
    }

    fn subscribe(&self) -> EventStream {
        let (tx, rx) = mpsc::unbounded_channel();
        if let Ok(mut slot) = self.event_tx.lock() {
            *slot = Some(tx);
        }
        rx
    }

    fn finish(&self) {
        let audio = self.buffer.lock().map(|b| b.clone()).unwrap_or_default();
        debug!("openai finish: {} samples @ {} Hz", audio.len(), self.sample_rate);
        let sample_rate = self.sample_rate;
        let model = self.model.clone();
        let api_key = self.api_key.clone();
        let client = self.client.clone();
        let event_tx = Arc::clone(&self.event_tx);
        let options = self.options.clone();

        // Runs on the tokio runtime (finish() is called from the async daemon loop).
        tokio::spawn(async move {
            let event =
                match transcribe(&client, &api_key, &model, &audio, sample_rate, &options).await {
                    Ok(text) => {
                        debug!("OpenAI transcription: '{}'", text);
                        TranscriptEvent::Final(text)
                    }
                    Err(e) => {
                        error!("OpenAI transcription failed: {}", e);
                        TranscriptEvent::Error(EngineError::Backend(e.to_string()))
                    }
                };
            if let Ok(guard) = event_tx.lock() {
                if let Some(tx) = guard.as_ref() {
                    let _ = tx.send(event);
                }
            }
        });
    }

    fn reset(&self) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.clear();
        }
        if let Ok(mut slot) = self.event_tx.lock() {
            *slot = None;
        }
    }

    fn get_audio_buffer(&self) -> Vec<i16> {
        self.buffer.lock().map(|b| b.clone()).unwrap_or_default()
    }

    /// Hosted output is already punctuated and cased, and vocabulary hints go
    /// in as `keywords`, so the local helper chain is not needed.
    fn default_stages(&self) -> Vec<Stage> {
        Vec::new()
    }
}

/// Read `OPENAI_API_KEY`, failing if it is unset or empty.
pub(crate) fn api_key_from_env() -> Result<String> {
    std::env::var("OPENAI_API_KEY").ok().filter(|k| !k.is_empty()).ok_or_else(|| {
        anyhow::anyhow!("OPENAI_API_KEY is not set; the openai engine cannot be used")
    })
}

/// POST the buffer as a WAV to the OpenAI transcription endpoint; return the text.
pub(crate) async fn transcribe(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    samples: &[i16],
    sample_rate: u32,
    options: &OpenAiOptions,
) -> Result<String> {
    let wav = encode_wav(samples, sample_rate)?;
    debug!(
        "openai upload: {} wav bytes ({} samples @ {} Hz)",
        wav.len(),
        samples.len(),
        sample_rate
    );

    let part = reqwest::multipart::Part::bytes(wav).file_name("audio.wav").mime_str("audio/wav")?;
    let mut form =
        reqwest::multipart::Form::new().text("model", model.to_string()).part("file", part);
    if supports_prompt(model) {
        if let Some(prompt) = &options.prompt {
            form = form.text("prompt", prompt.clone());
        }
    }
    if supports_context_lists(model) {
        for k in &options.keywords {
            form = form.text("keywords[]", k.clone());
        }
        for l in &options.languages {
            form = form.text("languages[]", l.clone());
        }
    }

    let resp = client
        .post(options.transcriptions_url.as_deref().unwrap_or(TRANSCRIPTIONS_URL))
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .await?;

    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("HTTP {}: {}", status, body);
    }

    // Default response_format is JSON: { "text": "..." }.
    let parsed: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("invalid transcription response: {e}: {body}"))?;
    let text = parsed.get("text").and_then(|t| t.as_str()).unwrap_or_default();
    Ok(text.to_string())
}

/// Encode i16 mono PCM as a 16-bit WAV in memory.
fn encode_wav(samples: &[i16], sample_rate: u32) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = Cursor::new(Vec::<u8>::new());
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec)?;
        for &s in samples {
            writer.write_sample(s)?;
        }
        writer.finalize()?;
    }
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_merge_and_filter_keywords() {
        let cfg = OpenAiConfig {
            extra_keywords: "WorkOS, JWKS ,ok".into(),
            languages: "EN, fr".into(),
            delay: "bogus".into(),
            ..OpenAiConfig::default()
        };
        let dict = vec![
            "workos".to_string(),
            "ad".to_string(),
            "bad<word".to_string(),
            "lifemd".to_string(),
        ];
        let o = cfg.to_options(&dict);
        // Case-insensitive dedupe keeps the first spelling; short and unsafe words drop.
        assert_eq!(o.keywords, vec!["WorkOS", "JWKS", "lifemd"]);
        assert_eq!(o.languages, vec!["en", "fr"]);
        assert_eq!(o.delay, "low");
        assert!(o.prompt.is_none());
        assert_eq!(o.realtime_url, default_realtime_url());
    }

    #[test]
    fn dictionary_keywords_can_be_disabled() {
        let cfg = OpenAiConfig { keywords_from_dictionary: false, ..OpenAiConfig::default() };
        assert!(cfg.to_options(&["lifemd".to_string()]).keywords.is_empty());
    }

    #[test]
    fn context_support_by_model() {
        assert!(supports_context_lists("gpt-transcribe"));
        assert!(supports_context_lists("gpt-live-transcribe"));
        assert!(!supports_context_lists("whisper-1"));
        assert!(supports_prompt("whisper-1"));
        assert!(!supports_prompt("gpt-4o-transcribe-diarize"));
    }

    #[test]
    fn encode_wav_has_riff_header_and_data() {
        let wav = encode_wav(&[0, 1, -1, 100, -100], 16000).unwrap();
        // RIFF/WAVE header + fmt + data chunk (44-byte header + 5*2 bytes).
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(wav.len(), 44 + 5 * 2);
    }

    #[test]
    fn new_fails_without_key() {
        // Only meaningful when the key is absent in the test environment.
        if std::env::var("OPENAI_API_KEY").map(|k| k.is_empty()).unwrap_or(true) {
            assert!(OpenAiEngine::new("gpt-transcribe".into(), 16000, OpenAiOptions::default())
                .is_err());
        }
    }
}
