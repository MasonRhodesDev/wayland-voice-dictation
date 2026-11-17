use anyhow::Result;
use ndarray::Array2;
use parakeet_rs::{Parakeet, ParakeetDecoder, TimestampMode, Transcriber};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

use crate::chunking::{transcribe_chunked_with_timestamps, ChunkConfig, TimestampedChunkResult};
use crate::engine::TranscriptionEngine;
use crate::hotword_trie::{self, HotwordTrie};

const MIN_AUDIO_SAMPLES: usize = 2400;
const RETRANSCRIBE_THRESHOLD: usize = 4800;

pub struct CtcEngine {
    parakeet: Arc<Mutex<Parakeet>>,
    #[allow(dead_code)]
    hotword_trie: Arc<HotwordTrie>,
    #[allow(dead_code)]
    beam_width: usize,
    audio_buffer: Arc<Mutex<Vec<i16>>>,
    sample_rate: u32,
    current_text: Arc<Mutex<String>>,
    last_transcribed_len: Arc<Mutex<usize>>,
    chunk_config: ChunkConfig,
}

impl CtcEngine {
    pub fn new(
        model_path: PathBuf,
        sample_rate: u32,
        hotwords_path: Option<PathBuf>,
        beam_width: usize,
    ) -> Result<Self> {
        info!("Loading Parakeet CTC model from {:?}", model_path);

        if sample_rate != 16000 {
            anyhow::bail!("Parakeet requires 16kHz audio, got {} Hz", sample_rate);
        }

        let parakeet = Parakeet::from_pretrained(model_path.to_str().unwrap_or("."), None)?;

        let tokenizer_path = model_path.join("tokenizer.json");
        let hotword_trie = load_hotword_trie(hotwords_path, &tokenizer_path)?;

        if beam_width > 0 && !hotword_trie.is_empty() {
            warn!(
                "Beam search (width={}) with {} hotwords requested but not yet active. \
                 Using greedy decoding. The beam search algorithm is implemented and tested \
                 but requires feature extraction support from parakeet-rs.",
                beam_width,
                hotword_trie.len()
            );
        }

        let chunk_config = ChunkConfig::new(30, 2, sample_rate);

        Ok(Self {
            parakeet: Arc::new(Mutex::new(parakeet)),
            hotword_trie: Arc::new(hotword_trie),
            beam_width,
            audio_buffer: Arc::new(Mutex::new(Vec::with_capacity(480_000))),
            sample_rate,
            current_text: Arc::new(Mutex::new(String::new())),
            last_transcribed_len: Arc::new(Mutex::new(0)),
            chunk_config,
        })
    }

    fn samples_to_f32(samples: &[i16]) -> Vec<f32> {
        samples.iter().map(|&s| s as f32 / 32768.0).collect()
    }

    fn transcribe_chunk(&self, samples: &[i16]) -> Result<String> {
        if samples.is_empty() {
            return Ok(String::new());
        }

        let f32_samples = Self::samples_to_f32(samples);
        let mut parakeet = self
            .parakeet
            .lock()
            .map_err(|e| anyhow::anyhow!("Parakeet CTC model lock poisoned: {}", e))?;
        let result = parakeet.transcribe_samples(f32_samples, self.sample_rate, 1, None)?;
        Ok(result.text)
    }

    fn transcribe_chunk_with_timestamps(&self, samples: &[i16]) -> Result<TimestampedChunkResult> {
        if samples.is_empty() {
            return Ok(TimestampedChunkResult { text: String::new(), words: Vec::new() });
        }

        let f32_samples = Self::samples_to_f32(samples);
        let mut parakeet = self
            .parakeet
            .lock()
            .map_err(|e| anyhow::anyhow!("Parakeet CTC model lock poisoned: {}", e))?;
        let result = parakeet.transcribe_samples(
            f32_samples,
            self.sample_rate,
            1,
            Some(TimestampMode::Words),
        )?;

        Ok(TimestampedChunkResult { text: result.text, words: result.tokens })
    }

    fn transcribe_buffer(&self, samples: &[i16]) -> Result<String> {
        if samples.is_empty() {
            debug!("transcribe_buffer: empty samples");
            return Ok(String::new());
        }

        let max_sample = samples.iter().map(|s| s.abs()).max().unwrap_or(0);
        let rms = (samples.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / samples.len() as f64)
            .sqrt();
        let duration_secs = samples.len() as f32 / self.sample_rate as f32;
        debug!(
            "transcribe_buffer (CTC): {} samples, max={}, rms={:.1}, duration={:.2}s",
            samples.len(),
            max_sample,
            rms,
            duration_secs
        );

        let normalized = normalize_audio(samples, 3000.0, 20.0);
        let samples = &normalized;

        if self.chunk_config.needs_chunking(samples) {
            return transcribe_chunked_with_timestamps(samples, &self.chunk_config, |chunk| {
                self.transcribe_chunk_with_timestamps(chunk)
            });
        }

        self.transcribe_chunk(samples)
    }
}

impl crate::stream_engine::LocalModel for CtcEngine {
    fn transcribe(&self, samples: &[i16]) -> anyhow::Result<String> {
        self.transcribe_buffer(samples)
    }
}

impl TranscriptionEngine for CtcEngine {
    fn process_audio(&self, samples: &[i16]) -> Result<()> {
        let mut buffer = self
            .audio_buffer
            .lock()
            .map_err(|e| anyhow::anyhow!("Audio buffer lock poisoned: {}", e))?;
        buffer.extend_from_slice(samples);
        Ok(())
    }

    fn get_current_text(&self) -> Result<String> {
        let buffer = self
            .audio_buffer
            .lock()
            .map_err(|e| anyhow::anyhow!("Audio buffer lock poisoned: {}", e))?;

        if buffer.is_empty() || buffer.len() < MIN_AUDIO_SAMPLES {
            return Ok(String::new());
        }

        let current_len = buffer.len();
        let last_len_val = {
            let last_len = self
                .last_transcribed_len
                .lock()
                .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
            *last_len
        };

        if current_len <= last_len_val + RETRANSCRIBE_THRESHOLD {
            let cached =
                self.current_text.lock().map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
            return Ok(cached.clone());
        }

        let full_audio = buffer.clone();
        drop(buffer);

        debug!(
            "CTC preview transcription: {} samples ({:.2}s)",
            full_audio.len(),
            full_audio.len() as f32 / 16000.0
        );

        let full_text = self.transcribe_buffer(&full_audio)?;

        {
            let mut cached =
                self.current_text.lock().map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
            *cached = full_text.clone();
        }
        {
            let mut last_len = self
                .last_transcribed_len
                .lock()
                .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
            *last_len = current_len;
        }

        Ok(full_text)
    }

    fn get_final_result(&self) -> Result<String> {
        let buffer = self
            .audio_buffer
            .lock()
            .map_err(|e| anyhow::anyhow!("Audio buffer lock poisoned: {}", e))?;
        let samples = buffer.clone();
        drop(buffer);
        self.transcribe_buffer(&samples)
    }

    fn get_cached_text(&self) -> String {
        self.current_text.lock().map(|guard| guard.clone()).unwrap_or_default()
    }

    fn get_audio_buffer(&self) -> Vec<i16> {
        self.audio_buffer.lock().map(|guard| guard.clone()).unwrap_or_default()
    }

    fn reset(&self) {
        if let Ok(mut buffer) = self.audio_buffer.lock() {
            buffer.clear();
        }
        if let Ok(mut text) = self.current_text.lock() {
            text.clear();
        }
        if let Ok(mut last_len) = self.last_transcribed_len.lock() {
            *last_len = 0;
        }
    }
}

#[derive(Debug, Clone)]
struct BeamHypothesis {
    tokens: Vec<u32>,
    blank_score: f32,
    non_blank_score: f32,
    hotword_context: Vec<u32>,
}

impl BeamHypothesis {
    fn total_score(&self) -> f32 {
        log_add(self.blank_score, self.non_blank_score)
    }
}

pub fn ctc_beam_search_decode(
    logits: &Array2<f32>,
    decoder: &ParakeetDecoder,
    hotword_trie: &HotwordTrie,
    beam_width: usize,
) -> Result<String> {
    let time_steps = logits.shape()[0];
    let vocab_size = logits.shape()[1];
    let blank_id = decoder.pad_token_id() as u32;

    let log_probs = log_softmax(logits);

    let initial = BeamHypothesis {
        tokens: Vec::new(),
        blank_score: 0.0,
        non_blank_score: f32::NEG_INFINITY,
        hotword_context: Vec::new(),
    };

    let mut beam = vec![initial];

    for t in 0..time_steps {
        let frame_log_probs = log_probs.row(t);

        let mut next_beam: std::collections::HashMap<Vec<u32>, BeamHypothesis> =
            std::collections::HashMap::new();

        for hyp in &beam {
            {
                let blank_log_prob = frame_log_probs[blank_id as usize];
                let new_blank_score =
                    log_add(hyp.blank_score, hyp.non_blank_score) + blank_log_prob;

                let entry = next_beam.entry(hyp.tokens.clone()).or_insert_with(|| BeamHypothesis {
                    tokens: hyp.tokens.clone(),
                    blank_score: f32::NEG_INFINITY,
                    non_blank_score: f32::NEG_INFINITY,
                    hotword_context: hyp.hotword_context.clone(),
                });
                entry.blank_score = log_add(entry.blank_score, new_blank_score);
            }

            let top_tokens =
                top_k_tokens(frame_log_probs.as_slice().unwrap(), beam_width * 3, blank_id);

            for &(token_id, token_log_prob) in &top_tokens {
                let last_token = hyp.tokens.last().copied();

                if Some(token_id) == last_token {
                    let new_non_blank_score = hyp.blank_score + token_log_prob;

                    let boost = if !hotword_trie.is_empty() {
                        hotword_trie.boost_for_token(&hyp.hotword_context, token_id)
                    } else {
                        0.0
                    };

                    let entry =
                        next_beam.entry(hyp.tokens.clone()).or_insert_with(|| BeamHypothesis {
                            tokens: hyp.tokens.clone(),
                            blank_score: f32::NEG_INFINITY,
                            non_blank_score: f32::NEG_INFINITY,
                            hotword_context: hyp.hotword_context.clone(),
                        });
                    entry.non_blank_score =
                        log_add(entry.non_blank_score, new_non_blank_score + boost);
                } else {
                    let new_non_blank_score =
                        log_add(hyp.blank_score, hyp.non_blank_score) + token_log_prob;

                    let mut new_tokens = hyp.tokens.clone();
                    new_tokens.push(token_id);

                    let mut new_context = hyp.hotword_context.clone();
                    new_context.push(token_id);

                    let boost = if !hotword_trie.is_empty() {
                        hotword_trie.boost_for_token(&hyp.hotword_context, token_id)
                    } else {
                        0.0
                    };

                    let trie_match = hotword_trie.query(&new_context);
                    if !trie_match.is_prefix && !trie_match.is_complete {
                        let single = hotword_trie.query(&[token_id]);
                        new_context = if single.is_prefix || single.is_complete {
                            vec![token_id]
                        } else {
                            Vec::new()
                        };
                    }

                    let entry =
                        next_beam.entry(new_tokens.clone()).or_insert_with(|| BeamHypothesis {
                            tokens: new_tokens,
                            blank_score: f32::NEG_INFINITY,
                            non_blank_score: f32::NEG_INFINITY,
                            hotword_context: new_context.clone(),
                        });
                    entry.non_blank_score =
                        log_add(entry.non_blank_score, new_non_blank_score + boost);
                }
            }
        }

        beam = next_beam.into_values().collect();
        beam.sort_by(|a, b| {
            b.total_score().partial_cmp(&a.total_score()).unwrap_or(std::cmp::Ordering::Equal)
        });
        beam.truncate(beam_width);
    }

    let best = beam
        .into_iter()
        .max_by(|a, b| {
            a.total_score().partial_cmp(&b.total_score()).unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(BeamHypothesis {
            tokens: Vec::new(),
            blank_score: f32::NEG_INFINITY,
            non_blank_score: f32::NEG_INFINITY,
            hotword_context: Vec::new(),
        });

    if best.tokens.is_empty() {
        return Ok(String::new());
    }

    let fake_logits = build_argmax_logits(&best.tokens, vocab_size);
    let text = decoder.decode(&fake_logits)?;

    Ok(text)
}

fn build_argmax_logits(token_ids: &[u32], vocab_size: usize) -> Array2<f32> {
    let num_frames = if token_ids.is_empty() { 1 } else { token_ids.len() * 2 - 1 };
    let blank_id = 1024;
    let mut logits = Array2::<f32>::from_elem((num_frames, vocab_size), -100.0);

    for (i, &token_id) in token_ids.iter().enumerate() {
        let frame = i * 2;
        logits[[frame, token_id as usize]] = 100.0;

        if i + 1 < token_ids.len() {
            let blank_frame = frame + 1;
            if (blank_id as usize) < vocab_size {
                logits[[blank_frame, blank_id as usize]] = 100.0;
            }
        }
    }

    logits
}

fn log_softmax(logits: &Array2<f32>) -> Array2<f32> {
    let time_steps = logits.shape()[0];
    let vocab_size = logits.shape()[1];
    let mut result = Array2::<f32>::zeros((time_steps, vocab_size));

    for t in 0..time_steps {
        let row = logits.row(t);
        let max_val = row.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let log_sum_exp: f32 = row.iter().map(|&x| (x - max_val).exp()).sum::<f32>().ln() + max_val;

        for v in 0..vocab_size {
            result[[t, v]] = logits[[t, v]] - log_sum_exp;
        }
    }

    result
}

fn log_add(a: f32, b: f32) -> f32 {
    if a == f32::NEG_INFINITY {
        return b;
    }
    if b == f32::NEG_INFINITY {
        return a;
    }
    let max = a.max(b);
    max + ((a - max).exp() + (b - max).exp()).ln()
}

fn top_k_tokens(log_probs: &[f32], k: usize, blank_id: u32) -> Vec<(u32, f32)> {
    let mut indexed: Vec<(u32, f32)> = log_probs
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != blank_id as usize)
        .map(|(i, &p)| (i as u32, p))
        .collect();

    indexed.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    indexed.truncate(k);
    indexed
}

fn load_hotword_trie(
    hotwords_path: Option<PathBuf>,
    tokenizer_path: &std::path::Path,
) -> Result<HotwordTrie> {
    let path = hotwords_path.unwrap_or_else(hotword_trie::default_hotwords_path);

    if !path.exists() {
        info!("No hotwords file at {}, hotword boosting disabled", path.display());
        return Ok(HotwordTrie::empty());
    }

    let entries = hotword_trie::parse_hotwords_file(&path)?;
    if entries.is_empty() {
        return Ok(HotwordTrie::empty());
    }

    let tokenizer = tokenizers::Tokenizer::from_file(tokenizer_path)
        .map_err(|e| anyhow::anyhow!("Failed to load tokenizer for hotwords: {}", e))?;

    let mut token_sequences: Vec<(hotword_trie::HotwordEntry, Vec<u32>)> = Vec::new();

    for entry in entries {
        match tokenizer.encode(entry.text.as_str(), false) {
            Ok(encoding) => {
                let ids: Vec<u32> = encoding.get_ids().to_vec();
                debug!("Hotword '{}' -> {} tokens: {:?}", entry.text, ids.len(), ids);
                token_sequences.push((entry, ids));
            }
            Err(e) => {
                warn!("Failed to tokenize hotword '{}': {}", entry.text, e);
            }
        }
    }

    Ok(HotwordTrie::from_token_sequences(&token_sequences))
}

fn normalize_audio(samples: &[i16], target_rms: f32, max_gain: f32) -> Vec<i16> {
    if samples.is_empty() {
        return Vec::new();
    }

    let rms = (samples.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / samples.len() as f64)
        .sqrt() as f32;

    if rms < 1.0 {
        return samples.to_vec();
    }

    let gain = (target_rms / rms).min(max_gain);

    if (gain - 1.0).abs() < 0.05 {
        return samples.to_vec();
    }

    debug!("normalize_audio: rms={:.1}, gain={:.2}x", rms, gain);

    samples
        .iter()
        .map(|&s| {
            let amplified = s as f32 * gain;
            amplified.clamp(i16::MIN as f32, i16::MAX as f32) as i16
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_add() {
        let result = log_add(0.0, 0.0);
        assert!((result - 2.0_f32.ln()).abs() < 1e-5);

        assert_eq!(log_add(f32::NEG_INFINITY, 1.0), 1.0);
        assert_eq!(log_add(1.0, f32::NEG_INFINITY), 1.0);
    }

    #[test]
    fn test_log_softmax() {
        let logits = Array2::from_shape_vec((1, 3), vec![1.0, 2.0, 3.0]).unwrap();
        let lsm = log_softmax(&logits);

        let sum: f32 = (0..3).map(|i| lsm[[0, i]].exp()).sum();
        assert!((sum - 1.0).abs() < 1e-5);

        assert!(lsm[[0, 2]] > lsm[[0, 1]]);
        assert!(lsm[[0, 1]] > lsm[[0, 0]]);
    }

    #[test]
    fn test_top_k_tokens() {
        let log_probs = vec![-5.0, -1.0, -3.0, -0.5, -2.0];
        let top = top_k_tokens(&log_probs, 3, 999);

        assert_eq!(top.len(), 3);
        assert_eq!(top[0].0, 3);
        assert_eq!(top[1].0, 1);
        assert_eq!(top[2].0, 4);
    }

    #[test]
    fn test_top_k_excludes_blank() {
        let log_probs = vec![-5.0, -1.0, 0.0];
        let top = top_k_tokens(&log_probs, 3, 2);
        assert!(top.iter().all(|(id, _)| *id != 2));
    }

    #[test]
    fn test_build_argmax_logits() {
        let tokens = vec![5, 10, 5];
        let logits = build_argmax_logits(&tokens, 20);

        assert_eq!(logits.shape(), &[5, 20]);

        let max_idx_0 = logits
            .row(0)
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
            .map(|(i, _)| i);
        assert_eq!(max_idx_0, Some(5));

        let max_idx_2 = logits
            .row(2)
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
            .map(|(i, _)| i);
        assert_eq!(max_idx_2, Some(10));
    }

    #[test]
    fn test_normalize_audio_silent() {
        let silent = vec![0i16; 100];
        let normalized = normalize_audio(&silent, 3000.0, 20.0);
        assert_eq!(normalized, silent);
    }

    #[test]
    fn test_normalize_audio_empty() {
        let normalized = normalize_audio(&[], 3000.0, 20.0);
        assert!(normalized.is_empty());
    }

    #[test]
    fn test_beam_search_synthetic_greedy_equivalent() {
        let vocab_size = 6;
        let blank_id: u32 = 5;

        let mut logits = Array2::<f32>::from_elem((6, vocab_size), -10.0);
        logits[[0, 0]] = 10.0;
        logits[[1, blank_id as usize]] = 10.0;
        logits[[2, 1]] = 10.0;
        logits[[3, blank_id as usize]] = 10.0;
        logits[[4, 2]] = 10.0;
        logits[[5, blank_id as usize]] = 10.0;

        let log_probs = log_softmax(&logits);
        let time_steps = log_probs.shape()[0];

        let initial = BeamHypothesis {
            tokens: Vec::new(),
            blank_score: 0.0,
            non_blank_score: f32::NEG_INFINITY,
            hotword_context: Vec::new(),
        };

        let _trie = HotwordTrie::empty();
        let beam_width = 5;
        let mut beam = vec![initial];

        for t in 0..time_steps {
            let frame_lp = log_probs.row(t);
            let mut next_beam: std::collections::HashMap<Vec<u32>, BeamHypothesis> =
                std::collections::HashMap::new();

            for hyp in &beam {
                {
                    let blp = frame_lp[blank_id as usize];
                    let new_bs = log_add(hyp.blank_score, hyp.non_blank_score) + blp;
                    let entry =
                        next_beam.entry(hyp.tokens.clone()).or_insert_with(|| BeamHypothesis {
                            tokens: hyp.tokens.clone(),
                            blank_score: f32::NEG_INFINITY,
                            non_blank_score: f32::NEG_INFINITY,
                            hotword_context: Vec::new(),
                        });
                    entry.blank_score = log_add(entry.blank_score, new_bs);
                }

                let top = top_k_tokens(frame_lp.as_slice().unwrap(), beam_width * 3, blank_id);
                for &(tid, tlp) in &top {
                    let last = hyp.tokens.last().copied();
                    if Some(tid) == last {
                        let new_nbs = hyp.blank_score + tlp;
                        let entry =
                            next_beam.entry(hyp.tokens.clone()).or_insert_with(|| BeamHypothesis {
                                tokens: hyp.tokens.clone(),
                                blank_score: f32::NEG_INFINITY,
                                non_blank_score: f32::NEG_INFINITY,
                                hotword_context: Vec::new(),
                            });
                        entry.non_blank_score = log_add(entry.non_blank_score, new_nbs);
                    } else {
                        let new_nbs = log_add(hyp.blank_score, hyp.non_blank_score) + tlp;
                        let mut new_tokens = hyp.tokens.clone();
                        new_tokens.push(tid);
                        let entry =
                            next_beam.entry(new_tokens.clone()).or_insert_with(|| BeamHypothesis {
                                tokens: new_tokens,
                                blank_score: f32::NEG_INFINITY,
                                non_blank_score: f32::NEG_INFINITY,
                                hotword_context: Vec::new(),
                            });
                        entry.non_blank_score = log_add(entry.non_blank_score, new_nbs);
                    }
                }
            }

            beam = next_beam.into_values().collect();
            beam.sort_by(|a, b| {
                b.total_score().partial_cmp(&a.total_score()).unwrap_or(std::cmp::Ordering::Equal)
            });
            beam.truncate(beam_width);
        }

        let best = beam
            .into_iter()
            .max_by(|a, b| {
                a.total_score().partial_cmp(&b.total_score()).unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap();

        assert_eq!(best.tokens, vec![0, 1, 2]);
    }

    #[test]
    fn test_beam_search_hotword_boost_effect() {
        use crate::hotword_trie::HotwordEntry;

        let vocab_size = 6;
        let blank_id: u32 = 5;

        let mut logits = Array2::<f32>::from_elem((4, vocab_size), -10.0);
        logits[[0, 0]] = 5.0;
        logits[[0, 3]] = 4.8;
        logits[[0, blank_id as usize]] = -5.0;
        logits[[1, blank_id as usize]] = 5.0;
        logits[[2, 1]] = 5.0;
        logits[[2, 4]] = 4.8;
        logits[[2, blank_id as usize]] = -5.0;
        logits[[3, blank_id as usize]] = 5.0;

        let entries =
            vec![(HotwordEntry { text: "hotword".to_string(), boost_score: 6.0 }, vec![3, 4])];
        let trie = HotwordTrie::from_token_sequences(&entries);

        let log_probs = log_softmax(&logits);
        let beam_width = 5;

        let initial = BeamHypothesis {
            tokens: Vec::new(),
            blank_score: 0.0,
            non_blank_score: f32::NEG_INFINITY,
            hotword_context: Vec::new(),
        };
        let mut beam = vec![initial];

        for t in 0..4 {
            let frame_lp = log_probs.row(t);
            let mut next_beam: std::collections::HashMap<Vec<u32>, BeamHypothesis> =
                std::collections::HashMap::new();

            for hyp in &beam {
                {
                    let blp = frame_lp[blank_id as usize];
                    let new_bs = log_add(hyp.blank_score, hyp.non_blank_score) + blp;
                    let entry =
                        next_beam.entry(hyp.tokens.clone()).or_insert_with(|| BeamHypothesis {
                            tokens: hyp.tokens.clone(),
                            blank_score: f32::NEG_INFINITY,
                            non_blank_score: f32::NEG_INFINITY,
                            hotword_context: hyp.hotword_context.clone(),
                        });
                    entry.blank_score = log_add(entry.blank_score, new_bs);
                }

                let top = top_k_tokens(frame_lp.as_slice().unwrap(), beam_width * 3, blank_id);
                for &(tid, tlp) in &top {
                    let last = hyp.tokens.last().copied();
                    if Some(tid) == last {
                        let new_nbs = hyp.blank_score + tlp;
                        let boost = trie.boost_for_token(&hyp.hotword_context, tid);
                        let entry =
                            next_beam.entry(hyp.tokens.clone()).or_insert_with(|| BeamHypothesis {
                                tokens: hyp.tokens.clone(),
                                blank_score: f32::NEG_INFINITY,
                                non_blank_score: f32::NEG_INFINITY,
                                hotword_context: hyp.hotword_context.clone(),
                            });
                        entry.non_blank_score = log_add(entry.non_blank_score, new_nbs + boost);
                    } else {
                        let new_nbs = log_add(hyp.blank_score, hyp.non_blank_score) + tlp;
                        let mut new_tokens = hyp.tokens.clone();
                        new_tokens.push(tid);
                        let mut new_context = hyp.hotword_context.clone();
                        new_context.push(tid);
                        let boost = trie.boost_for_token(&hyp.hotword_context, tid);
                        let trie_match = trie.query(&new_context);
                        if !trie_match.is_prefix && !trie_match.is_complete {
                            let single = trie.query(&[tid]);
                            new_context = if single.is_prefix || single.is_complete {
                                vec![tid]
                            } else {
                                Vec::new()
                            };
                        }
                        let entry =
                            next_beam.entry(new_tokens.clone()).or_insert_with(|| BeamHypothesis {
                                tokens: new_tokens,
                                blank_score: f32::NEG_INFINITY,
                                non_blank_score: f32::NEG_INFINITY,
                                hotword_context: new_context,
                            });
                        entry.non_blank_score = log_add(entry.non_blank_score, new_nbs + boost);
                    }
                }
            }

            beam = next_beam.into_values().collect();
            beam.sort_by(|a, b| {
                b.total_score().partial_cmp(&a.total_score()).unwrap_or(std::cmp::Ordering::Equal)
            });
            beam.truncate(beam_width);
        }

        let best = beam
            .into_iter()
            .max_by(|a, b| {
                a.total_score().partial_cmp(&b.total_score()).unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap();

        assert_eq!(best.tokens, vec![3, 4], "Hotword boost should have changed the result");
    }
}
