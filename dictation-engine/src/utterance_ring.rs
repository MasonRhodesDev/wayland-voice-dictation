//! Always-on ring of recent utterances for `voice-dictation retry`.
//!
//! Keeps the last N utterances (audio + final text) so a failed injection or a
//! bad transcript can be re-typed or re-transcribed. Independent of debug-audio
//! mode, and stored under `$XDG_STATE_HOME/voice-dictation/recent` with
//! user-only permissions. A size of 0 disables the ring: nothing is written.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::debug;

use crate::debug_audio::{create_private_file, list_recordings, prune_recordings, save_recording};

/// Number of utterances kept when the config does not say otherwise.
pub const DEFAULT_SIZE: usize = 5;

/// Metadata stored next to each utterance's WAV.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UtteranceMetadata {
    pub timestamp: DateTime<Utc>,
    pub duration_ms: u64,
    pub sample_rate: u32,
    /// Model spec that produced `final_text`.
    pub model: String,
    /// Engine transcript before post-processing.
    pub raw_text: String,
    /// Post-processed text, before per-app sanitization. Sanitization is
    /// re-applied at retry time for whichever window is focused then.
    pub final_text: String,
}

/// One stored utterance.
#[derive(Debug, Clone)]
pub struct Utterance {
    pub wav_path: PathBuf,
    pub metadata: UtteranceMetadata,
}

impl Utterance {
    /// Read the stored audio as mono 16-bit samples and its sample rate.
    pub fn read_audio(&self) -> Result<(Vec<i16>, u32)> {
        let mut reader = hound::WavReader::open(&self.wav_path)
            .with_context(|| format!("opening {}", self.wav_path.display()))?;
        let sample_rate = reader.spec().sample_rate;
        let samples = reader.samples::<i16>().collect::<std::result::Result<Vec<_>, _>>()?;
        Ok((samples, sample_rate))
    }
}

pub struct UtteranceRing {
    dir: PathBuf,
    size: usize,
}

impl UtteranceRing {
    /// Ring in the default state directory. With no resolvable state
    /// directory the ring is disabled.
    pub fn new(size: usize) -> Self {
        match default_dir() {
            Some(dir) => Self::with_dir(dir, size),
            None => Self::with_dir(PathBuf::new(), 0),
        }
    }

    pub fn with_dir(dir: PathBuf, size: usize) -> Self {
        Self { dir, size }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn is_enabled(&self) -> bool {
        self.size > 0
    }

    /// Store an utterance and drop the oldest beyond the ring size.
    /// Returns the WAV path, or None when the ring is disabled.
    pub fn push(&self, audio: &[i16], metadata: &UtteranceMetadata) -> Result<Option<PathBuf>> {
        if !self.is_enabled() {
            return Ok(None);
        }
        let wav_path =
            save_recording(&self.dir, metadata.timestamp, audio, metadata.sample_rate, metadata)?;
        prune_recordings(&self.dir, self.size)?;
        debug!("Stored utterance in retry ring: {}", wav_path.display());
        Ok(Some(wav_path))
    }

    /// Trim stored utterances to the ring size. With size 0 this removes
    /// everything left over from an earlier, larger setting.
    pub fn prune(&self) -> Result<()> {
        if self.dir.as_os_str().is_empty() || !self.dir.exists() {
            return Ok(());
        }
        prune_recordings(&self.dir, self.size)
    }

    /// The most recent utterance with readable metadata.
    pub fn latest(&self) -> Option<Utterance> {
        if !self.is_enabled() {
            return None;
        }
        list_recordings(&self.dir).into_iter().rev().find_map(|wav_path| {
            let json = fs::read_to_string(wav_path.with_extension("json")).ok()?;
            let metadata = serde_json::from_str(&json).ok()?;
            Some(Utterance { wav_path, metadata })
        })
    }

    /// Rewrite an utterance's metadata (e.g. after re-transcription), so a
    /// later plain `retry` re-types the new text.
    pub fn update(&self, utterance: &Utterance) -> Result<()> {
        let json = serde_json::to_string_pretty(&utterance.metadata)?;
        create_private_file(&utterance.wav_path.with_extension("json"))?
            .write_all(json.as_bytes())?;
        Ok(())
    }
}

/// `$XDG_STATE_HOME/voice-dictation/recent` (default `~/.local/state/...`).
pub fn default_dir() -> Option<PathBuf> {
    dirs::state_dir().map(|d| d.join("voice-dictation").join("recent"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn meta(text: &str, offset_ms: i64) -> UtteranceMetadata {
        UtteranceMetadata {
            timestamp: Utc::now() + chrono::Duration::milliseconds(offset_ms),
            duration_ms: 100,
            sample_rate: 16_000,
            model: "parakeet:default".to_string(),
            raw_text: text.to_lowercase(),
            final_text: text.to_string(),
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn keeps_only_the_last_n() {
        let tmp = tempfile::tempdir().unwrap();
        let ring = UtteranceRing::with_dir(tmp.path().join("recent"), 2);
        for (i, text) in ["One.", "Two.", "Three."].iter().enumerate() {
            ring.push(&[1, 2, 3], &meta(text, i as i64 * 10)).unwrap();
        }
        assert_eq!(list_recordings(ring.dir()).len(), 2);
        let jsons = fs::read_dir(ring.dir())
            .unwrap()
            .filter(|e| e.as_ref().unwrap().path().extension().unwrap() == "json")
            .count();
        assert_eq!(jsons, 2);
        assert_eq!(ring.latest().unwrap().metadata.final_text, "Three.");
    }

    #[test]
    fn size_zero_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("recent");
        let ring = UtteranceRing::with_dir(dir.clone(), 0);
        assert!(ring.push(&[1, 2, 3], &meta("Hello.", 0)).unwrap().is_none());
        assert!(!dir.exists());
        assert!(ring.latest().is_none());
    }

    #[test]
    fn size_zero_prune_clears_old_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("recent");
        UtteranceRing::with_dir(dir.clone(), 5).push(&[1], &meta("Old.", 0)).unwrap();
        UtteranceRing::with_dir(dir.clone(), 0).prune().unwrap();
        assert!(fs::read_dir(&dir).unwrap().next().is_none());
    }

    #[test]
    fn prune_removes_orphan_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("recent");
        let ring = UtteranceRing::with_dir(dir.clone(), 5);
        let kept = ring.push(&[1], &meta("Kept.", 0)).unwrap().unwrap();
        // Metadata whose WAV is gone (e.g. deleted by hand or a crash).
        let orphan = dir.join("recording_20000101_000000.000.json");
        fs::write(&orphan, "{\"final_text\": \"secret\"}").unwrap();

        ring.prune().unwrap();
        assert!(!orphan.exists());
        assert!(kept.exists() && kept.with_extension("json").exists());

        // Size 0 at startup must leave no transcript text behind either.
        fs::write(&orphan, "{}").unwrap();
        UtteranceRing::with_dir(dir.clone(), 0).prune().unwrap();
        assert!(fs::read_dir(&dir).unwrap().next().is_none());
    }

    #[test]
    fn failed_metadata_write_leaves_no_wav() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("recent");
        let m = meta("Hello.", 0);
        // Occupy the JSON path with a directory so the metadata write fails.
        let base = format!("recording_{}", m.timestamp.format("%Y%m%d_%H%M%S%.3f"));
        fs::create_dir_all(dir.join(format!("{base}.json"))).unwrap();

        let ring = UtteranceRing::with_dir(dir.clone(), 5);
        assert!(ring.push(&[1, 2, 3], &m).is_err());
        assert!(!dir.join(format!("{base}.wav")).exists());
        assert!(ring.latest().is_none());
    }

    #[test]
    fn storage_is_user_only() {
        let tmp = tempfile::tempdir().unwrap();
        let ring = UtteranceRing::with_dir(tmp.path().join("recent"), 5);
        let wav = ring.push(&[1, 2, 3], &meta("Hello.", 0)).unwrap().unwrap();
        assert_eq!(mode(ring.dir()), 0o700);
        assert_eq!(mode(&wav), 0o600);
        assert_eq!(mode(&wav.with_extension("json")), 0o600);
    }

    #[test]
    fn audio_and_metadata_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let ring = UtteranceRing::with_dir(tmp.path().join("recent"), 5);
        let m = meta("Hello world.", 0);
        ring.push(&[10, -20, 30], &m).unwrap();

        let mut latest = ring.latest().unwrap();
        assert_eq!(latest.metadata, m);
        assert_eq!(latest.read_audio().unwrap(), (vec![10, -20, 30], 16_000));

        latest.metadata.final_text = "Hello, world.".to_string();
        ring.update(&latest).unwrap();
        assert_eq!(ring.latest().unwrap().metadata.final_text, "Hello, world.");
        assert_eq!(mode(&latest.wav_path.with_extension("json")), 0o600);
    }
}
