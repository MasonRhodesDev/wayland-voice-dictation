use std::fmt;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as imp;

#[cfg(any(not(target_os = "linux"), not(feature = "correction")))]
mod no_correction;
#[cfg(any(not(target_os = "linux"), not(feature = "tray")))]
mod no_tray;

pub use imp::{
    audio_hotplug_dir, check_runtime_dependencies, copy_to_clipboard, daemon_exe, daemon_running,
    inhibit_idle, missing_permissions, notify_ready, notify_watchdog, pause_media_if_playing,
    request_permissions, resume_media, run_ime_probe, send_request, serve_control, spawn_overlay,
    spawn_tray, type_text_with_progress, ControlServer, CorrectionLearning, FocusTarget,
    IdleInhibitor, SERVICE_RESTART_HINT, SERVICE_START_HINT, SERVICE_STATUS_HINT,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permission {
    pub name: &'static str,
    pub remedy: String,
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} permission is missing: {}", self.name, self.remedy)
    }
}

#[derive(Debug, Clone)]
pub struct CorrectionSettings {
    pub enabled: bool,
    pub monitor_duration_secs: u64,
    pub auto_promote_threshold: u32,
    pub max_age_days: u32,
    pub accessibility_bridge: bool,
    pub wezterm_backend: bool,
}

#[derive(Debug, Clone)]
pub struct ImeProbeOptions {
    pub secs: u64,
    pub json: bool,
    pub commit_text: Option<String>,
    pub commit_class: Option<String>,
}
