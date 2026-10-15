use anyhow::Result;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::{oneshot, watch, Mutex};
use tracing::info;
use zbus::interface;

use crate::HealthState;

/// Daemon state enum shared between lib.rs and dbus_control.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    Idle,       // Waiting for StartRecording command, GUI hidden
    Recording,  // Actively recording audio and transcribing, GUI visible
    Processing, // Running transcription and typing, GUI visible with spinner
}

impl std::fmt::Display for DaemonState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonState::Idle => write!(f, "idle"),
            DaemonState::Recording => write!(f, "recording"),
            DaemonState::Processing => write!(f, "processing"),
        }
    }
}

/// D-Bus service interface for voice dictation control
pub struct VoiceDictationService {
    command_sender: Arc<Mutex<tokio::sync::mpsc::Sender<DaemonCommand>>>,
    state_receiver: watch::Receiver<DaemonState>,
    health_state: Arc<HealthState>,
}

/// Commands that can be sent from D-Bus to the daemon
#[derive(Debug, Clone)]
pub enum DaemonCommand {
    StartRecording,
    StopRecording,
    Confirm,
    Shutdown,
    /// Exit with code 64 so systemd (RestartForceExitStatus=64) restarts the daemon.
    Restart,
    /// Switch audio input device. None = system default, Some(name) = specific device.
    SwitchDevice(Option<String>),
    /// Re-arm correction monitoring against the last injection (captures late edits).
    SnapshotCorrection,
    /// Re-type the last utterance into the focused window, optionally
    /// re-transcribing its stored audio first (with `model` overriding the
    /// configured engine). The daemon answers on `reply` with the typed text.
    Retry {
        retranscribe: bool,
        model: Option<String>,
        reply: RetryReply,
    },
}

/// One-shot reply slot for [`DaemonCommand::Retry`]. Dropping it unanswered
/// (e.g. the daemon was busy) fails the D-Bus call.
#[derive(Debug, Clone)]
pub struct RetryReply(Arc<std::sync::Mutex<Option<oneshot::Sender<RetryResult>>>>);

/// Typed text on success, an error message for the CLI otherwise.
pub type RetryResult = Result<String, String>;

impl RetryReply {
    pub fn new() -> (Self, oneshot::Receiver<RetryResult>) {
        let (tx, rx) = oneshot::channel();
        (Self(Arc::new(std::sync::Mutex::new(Some(tx)))), rx)
    }

    pub fn send(&self, result: RetryResult) {
        if let Some(tx) = self.0.lock().ok().and_then(|mut slot| slot.take()) {
            let _ = tx.send(result);
        }
    }
}

/// Response from status query
#[derive(Debug, Clone)]
pub struct StatusInfo {
    pub state: String,
    pub session_active: bool,
}

#[interface(name = "com.voicedictation.Control")]
impl VoiceDictationService {
    /// Start a new recording session
    async fn start_recording(&self) -> zbus::fdo::Result<()> {
        info!("D-Bus: StartRecording called");
        let sender = self.command_sender.lock().await;
        sender
            .send(DaemonCommand::StartRecording)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("Failed to send command: {}", e)))?;
        Ok(())
    }

    /// Stop the current recording session (cancel)
    async fn stop_recording(&self) -> zbus::fdo::Result<()> {
        info!("D-Bus: StopRecording called");
        let sender = self.command_sender.lock().await;
        sender
            .send(DaemonCommand::StopRecording)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("Failed to send command: {}", e)))?;
        Ok(())
    }

    /// Confirm and finalize the current transcription
    async fn confirm(&self) -> zbus::fdo::Result<()> {
        info!("D-Bus: Confirm called");
        let sender = self.command_sender.lock().await;
        sender
            .send(DaemonCommand::Confirm)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("Failed to send command: {}", e)))?;
        Ok(())
    }

    /// Get current daemon status
    async fn status(&self) -> zbus::fdo::Result<(String, bool)> {
        info!("D-Bus: Status called");
        let state = *self.state_receiver.borrow();
        let session_active = state != DaemonState::Idle;
        Ok((state.to_string(), session_active))
    }

    /// Get health status of all subsystems
    async fn health_check(&self) -> zbus::fdo::Result<(String, String, String)> {
        info!("D-Bus: HealthCheck called");

        let gui_status = if self.health_state.gui_healthy.load(Ordering::Relaxed) {
            "healthy"
        } else {
            "unavailable"
        };

        let engine_status = if self.health_state.engine_healthy.load(Ordering::Relaxed) {
            "healthy"
        } else {
            "not loaded"
        };

        let audio_status = if self.health_state.audio_healthy.load(Ordering::Relaxed) {
            "healthy"
        } else {
            let state = *self.state_receiver.borrow();
            if state == DaemonState::Recording {
                "unhealthy"
            } else {
                "idle"
            }
        };

        Ok((gui_status.to_string(), engine_status.to_string(), audio_status.to_string()))
    }

    /// Re-arm correction monitoring against the last injection
    async fn snapshot_correction(&self) -> zbus::fdo::Result<()> {
        info!("D-Bus: SnapshotCorrection called");
        let sender = self.command_sender.lock().await;
        sender
            .send(DaemonCommand::SnapshotCorrection)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("Failed to send command: {}", e)))?;
        Ok(())
    }

    /// Re-type the last utterance into the focused window. With
    /// `retranscribe`, re-run its stored audio first; a non-empty `model`
    /// overrides the configured engine. Returns the typed text.
    async fn retry(&self, retranscribe: bool, model: String) -> zbus::fdo::Result<String> {
        info!("D-Bus: Retry called (retranscribe={}, model={:?})", retranscribe, model);
        let model = (!model.is_empty()).then_some(model);
        let (reply, rx) = RetryReply::new();
        {
            let sender = self.command_sender.lock().await;
            sender
                .send(DaemonCommand::Retry { retranscribe, model, reply })
                .await
                .map_err(|e| zbus::fdo::Error::Failed(format!("Failed to send command: {}", e)))?;
        }
        match rx.await {
            Ok(Ok(text)) => Ok(text),
            Ok(Err(e)) => Err(zbus::fdo::Error::Failed(e)),
            Err(_) => Err(zbus::fdo::Error::Failed(
                "Daemon is busy (recording or processing); retry when idle".to_string(),
            )),
        }
    }

    /// Shutdown the daemon gracefully
    async fn shutdown(&self) -> zbus::fdo::Result<()> {
        info!("D-Bus: Shutdown called");
        let sender = self.command_sender.lock().await;
        sender
            .send(DaemonCommand::Shutdown)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("Failed to send command: {}", e)))?;
        Ok(())
    }
}

/// Create and register D-Bus service
pub async fn create_dbus_service(
    state_receiver: watch::Receiver<DaemonState>,
    health_state: Arc<HealthState>,
) -> Result<(
    zbus::Connection,
    Arc<Mutex<tokio::sync::mpsc::Sender<DaemonCommand>>>,
    tokio::sync::mpsc::Receiver<DaemonCommand>,
)> {
    let (command_tx, command_rx) = tokio::sync::mpsc::channel(10);
    let command_sender = Arc::new(Mutex::new(command_tx));

    let service = VoiceDictationService {
        command_sender: Arc::clone(&command_sender),
        state_receiver,
        health_state,
    };

    // Live well-known name is com.voicedictation.Daemon. The interface is
    // com.voicedictation.Control (some surface docs label the interface).
    // Do not rename this bus name — CLI and keybinds depend on it.
    let connection = zbus::connection::Builder::session()?
        .name("com.voicedictation.Daemon")?
        .serve_at("/com/voicedictation/Control", service)?
        .build()
        .await?;

    info!("D-Bus service registered at com.voicedictation.Daemon");

    Ok((connection, command_sender, command_rx))
}
