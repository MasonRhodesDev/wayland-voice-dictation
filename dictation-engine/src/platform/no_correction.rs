use tracing::warn;

use crate::platform::CorrectionSettings;

pub struct CorrectionLearning;

impl CorrectionLearning {
    pub async fn start(_settings: &CorrectionSettings) -> Self {
        Self
    }

    pub fn record_injection(&mut self, _text: &str, _window_class: &str, _window_pid: Option<u32>) {
    }

    pub fn snapshot(&self) {
        warn!(
            "SnapshotCorrection requested but correction learning is not available in this build"
        );
    }
}
