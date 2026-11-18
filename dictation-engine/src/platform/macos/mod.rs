mod control;
mod focus;
mod keyboard;
mod permissions;
#[cfg(feature = "tray")]
mod tray;
mod ui;

use anyhow::{Context, Result};
use std::path::Path;

use crate::platform::ImeProbeOptions;

pub use super::no_correction::CorrectionLearning;
#[cfg(not(feature = "tray"))]
pub use super::no_tray::spawn_tray;
pub use control::{daemon_exe, daemon_running, send_request, serve_control, ControlServer};
pub use focus::FocusTarget;
pub use keyboard::type_text_with_progress;
pub use permissions::{missing_permissions, request_permissions};
#[cfg(feature = "tray")]
pub use tray::spawn_tray;
pub use ui::{run_daemon, spawn_overlay};

pub const SERVICE_STATUS_HINT: &str =
    "launchctl print gui/$(id -u)/com.voicedictation.daemon (log: ~/Library/Logs/voice-dictation.log)";
pub const SERVICE_START_HINT: &str = "launchctl kickstart gui/$(id -u)/com.voicedictation.daemon";
pub const SERVICE_RESTART_HINT: &str =
    "launchctl kickstart -k gui/$(id -u)/com.voicedictation.daemon";

pub struct IdleInhibitor {
    _caffeinate: tokio::process::Child,
}

pub async fn inhibit_idle(_reason: &str) -> Result<IdleInhibitor> {
    let caffeinate = tokio::process::Command::new("caffeinate")
        .args(["-i", "-w", &std::process::id().to_string()])
        .kill_on_drop(true)
        .spawn()
        .context("Failed to run caffeinate")?;
    Ok(IdleInhibitor { _caffeinate: caffeinate })
}

pub fn pause_media_if_playing() -> bool {
    false
}

pub fn resume_media() {}

pub fn notify_ready() -> Result<()> {
    Ok(())
}

pub fn notify_watchdog() -> Result<()> {
    Ok(())
}

pub fn copy_to_clipboard(_text: &str) -> Result<bool> {
    Ok(false)
}

pub fn audio_hotplug_dir() -> Option<&'static Path> {
    None
}

pub fn check_runtime_dependencies(_require_injector: bool, _require_display: bool) -> Result<()> {
    Ok(())
}

pub fn run_ime_probe(_opts: ImeProbeOptions) -> Result<()> {
    anyhow::bail!("ime-probe is only available on Wayland")
}
