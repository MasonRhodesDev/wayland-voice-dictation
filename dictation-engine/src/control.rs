use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tracing::{error, info};

use crate::platform::Permission;
use crate::HealthState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DaemonState {
    Idle,
    Recording,
    Processing,
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

impl std::str::FromStr for DaemonState {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "idle" => Ok(DaemonState::Idle),
            "recording" => Ok(DaemonState::Recording),
            "processing" => Ok(DaemonState::Processing),
            other => anyhow::bail!("unknown daemon state '{other}'"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonCommand {
    StartRecording,
    StopRecording,
    Confirm,
    Shutdown,
    Restart,
    SwitchDevice(Option<String>),
    SnapshotCorrection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlRequest {
    StartRecording,
    StopRecording,
    Confirm,
    Status,
    HealthCheck,
    SnapshotCorrection,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlReply {
    Ok,
    Status { state: DaemonState, session_active: bool },
    Health { gui: String, engine: String, audio: String },
    Error(String),
}

pub struct ControlContext {
    pub commands: mpsc::Sender<DaemonCommand>,
    pub state: watch::Receiver<DaemonState>,
    pub health: Arc<HealthState>,
    pub missing_permissions: fn() -> Vec<Permission>,
}

pub async fn handle(req: ControlRequest, ctx: &ControlContext) -> ControlReply {
    info!("Control request: {:?}", req);
    match req {
        ControlRequest::StartRecording => {
            let missing = (ctx.missing_permissions)();
            if missing.is_empty() {
                forward(ctx, DaemonCommand::StartRecording).await
            } else {
                let message =
                    missing.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ");
                error!("Cannot start recording: {}", message);
                ControlReply::Error(message)
            }
        }
        ControlRequest::StopRecording => forward(ctx, DaemonCommand::StopRecording).await,
        ControlRequest::Confirm => forward(ctx, DaemonCommand::Confirm).await,
        ControlRequest::SnapshotCorrection => forward(ctx, DaemonCommand::SnapshotCorrection).await,
        ControlRequest::Shutdown => forward(ctx, DaemonCommand::Shutdown).await,
        ControlRequest::Status => {
            let state = *ctx.state.borrow();
            ControlReply::Status { state, session_active: state != DaemonState::Idle }
        }
        ControlRequest::HealthCheck => health(ctx),
    }
}

async fn forward(ctx: &ControlContext, command: DaemonCommand) -> ControlReply {
    match ctx.commands.send(command).await {
        Ok(()) => ControlReply::Ok,
        Err(e) => ControlReply::Error(format!("Failed to send command: {}", e)),
    }
}

fn health(ctx: &ControlContext) -> ControlReply {
    let gui =
        if ctx.health.gui_healthy.load(Ordering::Relaxed) { "healthy" } else { "unavailable" };
    let engine =
        if ctx.health.engine_healthy.load(Ordering::Relaxed) { "healthy" } else { "not loaded" };
    let audio = if ctx.health.audio_healthy.load(Ordering::Relaxed) {
        "healthy"
    } else if *ctx.state.borrow() == DaemonState::Recording {
        "unhealthy"
    } else {
        "idle"
    };
    ControlReply::Health { gui: gui.into(), engine: engine.into(), audio: audio.into() }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn context(
        missing_permissions: fn() -> Vec<Permission>,
    ) -> (ControlContext, mpsc::Receiver<DaemonCommand>, watch::Sender<DaemonState>) {
        let (commands, command_rx) = mpsc::channel(4);
        let (state_tx, state) = watch::channel(DaemonState::Idle);
        let ctx = ControlContext {
            commands,
            state,
            health: Arc::new(HealthState::new()),
            missing_permissions,
        };
        (ctx, command_rx, state_tx)
    }

    fn granted() -> Vec<Permission> {
        Vec::new()
    }

    fn microphone_denied() -> Vec<Permission> {
        vec![Permission { name: "Microphone", remedy: "allow it".into() }]
    }

    #[tokio::test]
    async fn start_is_queued_when_permissions_are_granted() {
        let (ctx, mut rx, _state) = context(granted);
        assert_eq!(handle(ControlRequest::StartRecording, &ctx).await, ControlReply::Ok);
        assert!(matches!(rx.try_recv(), Ok(DaemonCommand::StartRecording)));
    }

    #[tokio::test]
    async fn start_is_refused_with_the_missing_permission() {
        let (ctx, mut rx, _state) = context(microphone_denied);
        let ControlReply::Error(message) = handle(ControlRequest::StartRecording, &ctx).await
        else {
            panic!("expected an error reply");
        };
        assert!(message.contains("Microphone"), "{message}");
        assert!(rx.try_recv().is_err(), "nothing may be queued");
    }

    #[tokio::test]
    async fn status_reflects_the_daemon_state() {
        let (ctx, _rx, state) = context(granted);
        state.send(DaemonState::Recording).unwrap();
        assert_eq!(
            handle(ControlRequest::Status, &ctx).await,
            ControlReply::Status { state: DaemonState::Recording, session_active: true }
        );
    }

    #[tokio::test]
    async fn health_reports_unhealthy_audio_only_while_recording() {
        let (ctx, _rx, state) = context(granted);
        let audio = |reply| match reply {
            ControlReply::Health { audio, .. } => audio,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(audio(handle(ControlRequest::HealthCheck, &ctx).await), "idle");
        state.send(DaemonState::Recording).unwrap();
        assert_eq!(audio(handle(ControlRequest::HealthCheck, &ctx).await), "unhealthy");
    }

    #[test]
    fn daemon_state_round_trips_through_its_wire_name() {
        for state in [DaemonState::Idle, DaemonState::Recording, DaemonState::Processing] {
            assert_eq!(state.to_string().parse::<DaemonState>().unwrap(), state);
            assert_eq!(serde_json::to_string(&state).unwrap(), format!("\"{state}\""));
        }
    }
}
