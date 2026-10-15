//! Debug audio preservation
//!
//! Saves audio recordings with metadata when debug mode is enabled.

use anyhow::Result;
use chrono::{DateTime, Utc};
use hound::{SampleFormat, WavSpec, WavWriter};
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Debug directory for audio files
const DEBUG_DIR: &str = "/tmp/voice-dictation-debug";

/// Maximum number of debug files to keep
const MAX_DEBUG_FILES: usize = 50;

/// Metadata for a debug audio recording
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

/// Check if debug audio is enabled via environment or config
pub fn is_debug_audio_enabled() -> bool {
    // Check RUST_LOG for debug level
    if let Ok(log_level) = std::env::var("RUST_LOG") {
        if log_level.contains("debug") || log_level.contains("trace") {
            return true;
        }
    }

    // Check explicit debug audio flag
    std::env::var("VOICE_DICTATION_DEBUG_AUDIO")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false)
}

/// Save audio buffer and metadata to debug directory
pub fn save_debug_audio(
    audio_buffer: &[i16],
    sample_rate: u32,
    metadata: AudioMetadata,
) -> Result<PathBuf> {
    let debug_dir = PathBuf::from(DEBUG_DIR);
    let wav_path =
        save_recording(&debug_dir, metadata.timestamp, audio_buffer, sample_rate, &metadata)?;

    info!(
        "Debug audio saved: {} ({:.2}s, {} samples)",
        wav_path.display(),
        audio_buffer.len() as f32 / sample_rate as f32,
        audio_buffer.len()
    );

    // Cleanup old files
    prune_recordings(&debug_dir, MAX_DEBUG_FILES)?;

    Ok(wav_path)
}

/// Write `recording_<timestamp>.wav` and its `.json` metadata into `dir`.
///
/// The directory is created `0700` and both files `0600`: recordings are
/// private voice audio. Returns the WAV path.
pub(crate) fn save_recording<M: Serialize>(
    dir: &Path,
    timestamp: DateTime<Utc>,
    audio_buffer: &[i16],
    sample_rate: u32,
    metadata: &M,
) -> Result<PathBuf> {
    ensure_private_dir(dir)?;

    // Generate filename from timestamp. The format sorts lexicographically,
    // so the newest recording is the last file name.
    let timestamp_str = timestamp.format("%Y%m%d_%H%M%S%.3f");
    let base_name = format!("recording_{}", timestamp_str);

    let wav_path = dir.join(format!("{}.wav", base_name));
    let json_path = dir.join(format!("{}.json", base_name));

    // Write WAV file
    let spec =
        WavSpec { channels: 1, sample_rate, bits_per_sample: 16, sample_format: SampleFormat::Int };

    let mut writer = WavWriter::new(BufWriter::new(create_private_file(&wav_path)?), spec)?;
    for &sample in audio_buffer {
        writer.write_sample(sample)?;
    }
    writer.finalize()?;

    // Write metadata JSON
    let json_content = serde_json::to_string_pretty(metadata)?;
    create_private_file(&json_path)?.write_all(json_content.as_bytes())?;

    Ok(wav_path)
}

/// Create `dir` (and parents) if needed and restrict it to the owner.
fn ensure_private_dir(dir: &Path) -> Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Open a new file readable and writable by the owner only.
pub(crate) fn create_private_file(path: &Path) -> Result<File> {
    Ok(OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?)
}

/// WAV recordings in `dir`, oldest first (by timestamped file name).
pub(crate) fn list_recordings(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut wavs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|ext| ext == "wav").unwrap_or(false))
        .collect();
    wavs.sort();
    wavs
}

/// Remove old recordings, keeping only the most recent `keep` WAV/JSON pairs
pub(crate) fn prune_recordings(dir: &Path, keep: usize) -> Result<()> {
    let wav_files = list_recordings(dir);

    if wav_files.len() <= keep {
        return Ok(());
    }

    // Remove oldest files
    let to_remove = wav_files.len() - keep;
    for wav_path in wav_files.into_iter().take(to_remove) {
        let json_path = wav_path.with_extension("json");

        if let Err(e) = fs::remove_file(&wav_path) {
            warn!("Failed to remove old recording WAV: {}", e);
        } else {
            debug!("Removed old recording: {}", wav_path.display());
        }

        if json_path.exists() {
            let _ = fs::remove_file(&json_path);
        }
    }

    Ok(())
}
