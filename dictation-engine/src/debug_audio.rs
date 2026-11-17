use anyhow::Result;
use chrono::{DateTime, Utc};
use hound::{SampleFormat, WavSpec, WavWriter};
use serde::Serialize;
use std::fs;
use std::path::PathBuf;
use tracing::{debug, info, warn};

const DEBUG_DIR: &str = "/tmp/voice-dictation-debug";

const MAX_DEBUG_FILES: usize = 50;

#[derive(Debug, Serialize)]
pub struct AudioMetadata {
    pub timestamp: DateTime<Utc>,
    pub duration_ms: u64,
    pub sample_rate: u32,
    pub sample_count: usize,
    pub devices: Vec<String>,
    pub active_device: Option<String>,
    pub preview_text: String,
    pub final_text: String,
    pub preview_engine: String,
    pub accurate_engine: String,
    pub same_model_used: bool,
}

pub fn is_debug_audio_enabled() -> bool {
    if let Ok(log_level) = std::env::var("RUST_LOG") {
        if log_level.contains("debug") || log_level.contains("trace") {
            return true;
        }
    }

    std::env::var("VOICE_DICTATION_DEBUG_AUDIO")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false)
}

pub fn save_debug_audio(
    audio_buffer: &[i16],
    sample_rate: u32,
    metadata: AudioMetadata,
) -> Result<PathBuf> {
    let debug_dir = PathBuf::from(DEBUG_DIR);
    fs::create_dir_all(&debug_dir)?;

    let timestamp_str = metadata.timestamp.format("%Y%m%d_%H%M%S%.3f");
    let base_name = format!("recording_{}", timestamp_str);

    let wav_path = debug_dir.join(format!("{}.wav", base_name));
    let json_path = debug_dir.join(format!("{}.json", base_name));

    let spec =
        WavSpec { channels: 1, sample_rate, bits_per_sample: 16, sample_format: SampleFormat::Int };

    let mut writer = WavWriter::create(&wav_path, spec)?;
    for &sample in audio_buffer {
        writer.write_sample(sample)?;
    }
    writer.finalize()?;

    let json_content = serde_json::to_string_pretty(&metadata)?;
    fs::write(&json_path, json_content)?;

    info!(
        "Debug audio saved: {} ({:.2}s, {} samples)",
        wav_path.display(),
        audio_buffer.len() as f32 / sample_rate as f32,
        audio_buffer.len()
    );

    cleanup_old_files(&debug_dir)?;

    Ok(wav_path)
}

fn cleanup_old_files(debug_dir: &PathBuf) -> Result<()> {
    let mut wav_files: Vec<_> = fs::read_dir(debug_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|ext| ext == "wav").unwrap_or(false))
        .collect();

    if wav_files.len() <= MAX_DEBUG_FILES {
        return Ok(());
    }

    wav_files.sort_by(|a, b| {
        let a_time = a.metadata().and_then(|m| m.modified()).ok();
        let b_time = b.metadata().and_then(|m| m.modified()).ok();
        a_time.cmp(&b_time)
    });

    let to_remove = wav_files.len() - MAX_DEBUG_FILES;
    for entry in wav_files.into_iter().take(to_remove) {
        let wav_path = entry.path();
        let json_path = wav_path.with_extension("json");

        if let Err(e) = fs::remove_file(&wav_path) {
            warn!("Failed to remove old debug WAV: {}", e);
        } else {
            debug!("Removed old debug file: {}", wav_path.display());
        }

        if json_path.exists() {
            let _ = fs::remove_file(&json_path);
        }
    }

    Ok(())
}
