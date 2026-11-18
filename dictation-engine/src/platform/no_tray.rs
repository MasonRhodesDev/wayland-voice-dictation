use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::audio_backend::BackendType;
use crate::control::{DaemonCommand, DaemonState};

pub fn spawn_tray(
    _state_rx: watch::Receiver<DaemonState>,
    _command_tx: mpsc::Sender<DaemonCommand>,
    _backend_type: BackendType,
    _initial_device: Option<String>,
) -> Option<JoinHandle<()>> {
    None
}
