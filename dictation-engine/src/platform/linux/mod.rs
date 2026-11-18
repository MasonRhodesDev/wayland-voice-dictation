mod control;
#[cfg(feature = "correction")]
pub(super) mod correction;
mod focus;
mod idle;
mod ime_probe;
mod keyboard;
#[cfg(feature = "tray")]
mod tray;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use systemd::daemon::{notify, STATE_READY, STATE_WATCHDOG};
use tokio::sync::{broadcast, mpsc};
use tracing::info;

use crate::platform::{Permission, WeztermInstall};
use crate::{GuiControl, GuiStatus};

#[cfg(not(feature = "tray"))]
pub use super::no_tray::spawn_tray;
pub use control::{daemon_exe, daemon_running, send_request, serve_control, ControlServer};
pub use focus::FocusTarget;
pub use idle::{inhibit_idle, IdleInhibitor};
pub use ime_probe::run as run_ime_probe;
pub use keyboard::type_text_with_progress;
#[cfg(feature = "tray")]
pub use tray::spawn_tray;

pub const SERVICE_STATUS_HINT: &str = "systemctl --user status voice-dictation";
pub const SERVICE_START_HINT: &str = "systemctl --user start voice-dictation";
pub const SERVICE_RESTART_HINT: &str = "systemctl --user restart voice-dictation";

pub struct PausedMedia;

pub fn pause_media_if_playing() -> Option<PausedMedia> {
    let output = Command::new("playerctl").arg("status").output().ok()?;
    if !String::from_utf8_lossy(&output.stdout).contains("Playing") {
        return None;
    }
    let _ = Command::new("playerctl").arg("pause").output();
    info!("Paused media playback");
    Some(PausedMedia)
}

pub fn resume_media(_paused: PausedMedia) {
    let _ = Command::new("playerctl").arg("play").output();
    info!("Resumed media playback");
}

pub fn notify_ready() -> Result<()> {
    notify(false, [(STATE_READY, "1")].iter())?;
    Ok(())
}

pub fn notify_watchdog() -> Result<()> {
    notify(false, [(STATE_WATCHDOG, "1")].iter())?;
    Ok(())
}

pub fn spawn_overlay(
    gui_control_tx: broadcast::Sender<GuiControl>,
    spectrum_tx: broadcast::Sender<Vec<f32>>,
    gui_status_tx: mpsc::Sender<GuiStatus>,
) -> Option<tokio::task::JoinHandle<()>> {
    let runtime_handle = tokio::runtime::Handle::current();
    Some(tokio::task::spawn_blocking(move || {
        let _ =
            slint_gui::run_integrated(gui_control_tx, spectrum_tx, gui_status_tx, runtime_handle);
    }))
}

pub fn copy_to_clipboard(text: &str) -> Result<bool> {
    tokio::process::Command::new("wl-copy")
        .arg(text)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("Failed to run wl-copy")?;
    Ok(true)
}

pub fn audio_hotplug_dir() -> Option<&'static Path> {
    Some(Path::new("/dev/snd"))
}

pub fn wezterm_install() -> Option<WeztermInstall> {
    Some(WeztermInstall {
        program: PathBuf::from("wezterm"),
        socket_dir: xdg_paths::BaseDirs::from_env().ok()?.runtime_path("wezterm"),
    })
}

pub fn missing_permissions() -> Vec<Permission> {
    Vec::new()
}

pub fn request_permissions() -> Vec<Permission> {
    Vec::new()
}

fn check_command_available(cmd: &str) -> bool {
    Command::new("which").arg(cmd).output().map(|output| output.status.success()).unwrap_or(false)
}

pub fn check_runtime_dependencies(require_injector: bool, require_display: bool) -> Result<()> {
    let mut missing = Vec::new();
    let mut warnings = Vec::new();

    if require_injector && !check_command_available("wtype") {
        missing.push("wtype - required for keyboard input injection");
    }

    if require_display && std::env::var("WAYLAND_DISPLAY").is_err() {
        if std::env::var("DISPLAY").is_ok() {
            missing.push("Wayland compositor - X11 detected but Wayland is required");
        } else {
            missing.push("Wayland compositor - no display server detected");
        }
    }

    if !check_command_available("pactl") && !check_command_available("pw-cli") {
        warnings.push("pactl or pw-cli - audio device enumeration may not work");
    }

    if !warnings.is_empty() {
        eprintln!("Warnings:");
        for warning in warnings {
            eprintln!("  - {}", warning);
        }
        eprintln!();
    }

    if !missing.is_empty() {
        eprintln!("Missing required runtime dependencies:");
        for dep in missing {
            eprintln!("  - {}", dep);
        }
        eprintln!();
        eprintln!("Install missing dependencies:");
        eprintln!("  Arch: sudo pacman -S wtype pipewire");
        eprintln!("  Fedora: sudo dnf install wtype pipewire");
        anyhow::bail!("Missing runtime dependencies");
    }

    Ok(())
}
