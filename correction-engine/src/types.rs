use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct InjectionContext {
    pub text: String,
    pub timestamp: DateTime<Utc>,
    pub instant: Instant,
    pub window_class: String,
    pub window_title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CorrectionPair {
    pub original: String,
    pub corrected: String,
    pub context_before: String,
    pub context_after: String,
    pub window_class: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CorrectionRecord {
    pub original: String,
    pub corrected: String,
    pub count: u32,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub promoted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockedPair {
    pub original: String,
    pub corrected: String,
    pub blocked_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub struct CorrectionStats {
    pub total_corrections: usize,
    pub total_observations: u32,
    pub promoted_count: usize,
    pub pending_count: usize,
}

#[derive(Debug, Clone)]
pub struct MonitorConfig {
    pub enabled: bool,
    pub monitor_duration_secs: u64,
    pub auto_promote_threshold: u32,
    pub max_age_days: u32,
    pub store_path: PathBuf,
    pub substitutions_path: PathBuf,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        let vd_dir = xdg_paths::ConfigDirs::from_env()
            .map(|d| d.data_dir("voice-dictation"))
            .unwrap_or_default();

        Self {
            enabled: true,
            monitor_duration_secs: 60,
            auto_promote_threshold: 3,
            max_age_days: 30,
            store_path: vd_dir.join("corrections.json"),
            substitutions_path: vd_dir.join("substitutions.txt"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TextChangeEvent {
    pub operation: TextChangeOp,
    pub start_pos: i32,
    pub length: i32,
    pub text: String,
    pub timestamp: Instant,
    pub source_app: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextChangeOp {
    Insert,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplacementEvent {
    pub deleted_text: String,
    pub inserted_text: String,
    pub position: i32,
    pub timestamp: Instant,
}
