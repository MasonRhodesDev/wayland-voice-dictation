use anyhow::Result;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, error};

use crate::post_processing::{Stage, LOCAL_MODEL_STAGES};

const MIN_AUDIO_SAMPLES: usize = 2400;
const RETRANSCRIBE_THRESHOLD: usize = 4800;
const PARTIAL_TICK: Duration = Duration::from_millis(200);

#[derive(Debug, Clone)]
pub enum TranscriptEvent {
    Partial(String),
    Final(String),
    Error(EngineError),
}

#[derive(Debug, Clone)]
pub enum EngineError {
    Transcription(String),
    Backend(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Transcription(m) => write!(f, "transcription error: {m}"),
            EngineError::Backend(m) => write!(f, "backend error: {m}"),
        }
    }
}
impl std::error::Error for EngineError {}

pub type EventStream = tokio_mpsc::UnboundedReceiver<TranscriptEvent>;

pub trait StreamingEngine: Send + Sync {
    fn process_audio(&self, samples: &[i16]) -> Result<()>;

    fn subscribe(&self) -> EventStream;

    fn finish(&self);

    fn reset(&self);

    fn get_audio_buffer(&self) -> Vec<i16>;

    fn default_stages(&self) -> Vec<Stage>;
}

pub trait LocalModel: Send + Sync {
    fn transcribe(&self, samples: &[i16]) -> Result<String>;
}

enum Ctrl {
    Audio,
    Finish,
    Reset,
    Shutdown,
}

pub struct LocalEngineDriver {
    buffer: Arc<Mutex<Vec<i16>>>,
    event_tx: Arc<Mutex<Option<tokio_mpsc::UnboundedSender<TranscriptEvent>>>>,
    ctrl_tx: Sender<Ctrl>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl LocalEngineDriver {
    pub fn new(model: Arc<dyn LocalModel>) -> Self {
        let buffer: Arc<Mutex<Vec<i16>>> = Arc::new(Mutex::new(Vec::new()));
        let event_tx: Arc<Mutex<Option<tokio_mpsc::UnboundedSender<TranscriptEvent>>>> =
            Arc::new(Mutex::new(None));
        let (ctrl_tx, ctrl_rx) = unbounded();

        let worker = {
            let buffer = Arc::clone(&buffer);
            let event_tx = Arc::clone(&event_tx);
            std::thread::Builder::new()
                .name("local-engine-driver".into())
                .spawn(move || run_worker(model, buffer, event_tx, ctrl_rx))
                .expect("failed to spawn LocalEngineDriver worker thread")
        };

        Self { buffer, event_tx, ctrl_tx, worker: Mutex::new(Some(worker)) }
    }
}

impl StreamingEngine for LocalEngineDriver {
    fn process_audio(&self, samples: &[i16]) -> Result<()> {
        self.buffer
            .lock()
            .map_err(|e| anyhow::anyhow!("audio buffer lock poisoned: {e}"))?
            .extend_from_slice(samples);
        let _ = self.ctrl_tx.send(Ctrl::Audio);
        Ok(())
    }

    fn subscribe(&self) -> EventStream {
        let (tx, rx) = tokio_mpsc::unbounded_channel();
        if let Ok(mut slot) = self.event_tx.lock() {
            *slot = Some(tx);
        }
        rx
    }

    fn finish(&self) {
        let _ = self.ctrl_tx.send(Ctrl::Finish);
    }

    fn reset(&self) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.clear();
        }
        if let Ok(mut slot) = self.event_tx.lock() {
            *slot = None;
        }
        let _ = self.ctrl_tx.send(Ctrl::Reset);
    }

    fn get_audio_buffer(&self) -> Vec<i16> {
        self.buffer.lock().map(|b| b.clone()).unwrap_or_default()
    }

    fn default_stages(&self) -> Vec<Stage> {
        LOCAL_MODEL_STAGES.to_vec()
    }
}

impl Drop for LocalEngineDriver {
    fn drop(&mut self) {
        let _ = self.ctrl_tx.send(Ctrl::Shutdown);
        if let Ok(mut w) = self.worker.lock() {
            if let Some(handle) = w.take() {
                let _ = handle.join();
            }
        }
    }
}

fn emit(
    slot: &Mutex<Option<tokio_mpsc::UnboundedSender<TranscriptEvent>>>,
    event: TranscriptEvent,
) {
    if let Ok(guard) = slot.lock() {
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(event);
        }
    }
}

fn transcribe_catching_panics(model: &dyn LocalModel, audio: &[i16]) -> Result<String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| model.transcribe(audio)))
        .unwrap_or_else(|panic| {
            let message = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            anyhow::bail!("model panicked on {} samples: {message}", audio.len())
        })
}

fn run_worker(
    model: Arc<dyn LocalModel>,
    buffer: Arc<Mutex<Vec<i16>>>,
    event_tx: Arc<Mutex<Option<tokio_mpsc::UnboundedSender<TranscriptEvent>>>>,
    ctrl_rx: Receiver<Ctrl>,
) {
    let mut last_len: usize = 0;

    loop {
        match ctrl_rx.recv_timeout(PARTIAL_TICK) {
            Ok(Ctrl::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,

            Ok(Ctrl::Reset) => last_len = 0,

            Ok(Ctrl::Finish) => {
                let audio = match buffer.lock() {
                    Ok(b) => b.clone(),
                    Err(_) => {
                        error!("driver: audio buffer lock poisoned; stopping worker");
                        break;
                    }
                };
                match transcribe_catching_panics(model.as_ref(), &audio) {
                    Ok(text) => {
                        debug!("driver: final at {} samples -> {:?}", audio.len(), text);
                        emit(&event_tx, TranscriptEvent::Final(text));
                    }
                    Err(e) => emit(
                        &event_tx,
                        TranscriptEvent::Error(EngineError::Transcription(e.to_string())),
                    ),
                }
                last_len = 0;
            }

            Ok(Ctrl::Audio) | Err(RecvTimeoutError::Timeout) => {
                let pending = {
                    let Ok(b) = buffer.lock() else {
                        error!("driver: audio buffer lock poisoned; stopping worker");
                        break;
                    };
                    let cur = b.len();
                    if cur < MIN_AUDIO_SAMPLES || cur <= last_len + RETRANSCRIBE_THRESHOLD {
                        None
                    } else {
                        Some((b.clone(), cur))
                    }
                };

                if let Some((audio, cur)) = pending {
                    match transcribe_catching_panics(model.as_ref(), &audio) {
                        Ok(text) => {
                            last_len = cur;
                            debug!("driver: partial at {} samples -> {:?}", cur, text);
                            emit(&event_tx, TranscriptEvent::Partial(text));
                        }
                        Err(e) => emit(
                            &event_tx,
                            TranscriptEvent::Error(EngineError::Transcription(e.to_string())),
                        ),
                    }
                }
            }
        }
    }

    debug!("LocalEngineDriver worker exiting");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{timeout, Duration as TDuration};

    struct CountModel {
        calls: Arc<AtomicUsize>,
    }
    impl LocalModel for CountModel {
        fn transcribe(&self, samples: &[i16]) -> Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(format!("len={}", samples.len()))
        }
    }

    struct FailModel;
    impl LocalModel for FailModel {
        fn transcribe(&self, _samples: &[i16]) -> Result<String> {
            anyhow::bail!("boom")
        }
    }

    struct PanicOnceModel {
        calls: AtomicUsize,
    }
    impl LocalModel for PanicOnceModel {
        fn transcribe(&self, samples: &[i16]) -> Result<String> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("model panicked");
            }
            Ok(format!("len={}", samples.len()))
        }
    }

    const RECV_TIMEOUT: TDuration = TDuration::from_secs(3);

    #[tokio::test]
    async fn emits_partial_then_final() {
        let calls = Arc::new(AtomicUsize::new(0));
        let driver = LocalEngineDriver::new(Arc::new(CountModel { calls: calls.clone() }));
        let mut rx = driver.subscribe();

        driver.process_audio(&[0i16; MIN_AUDIO_SAMPLES + RETRANSCRIBE_THRESHOLD + 1]).unwrap();

        let ev =
            timeout(RECV_TIMEOUT, rx.recv()).await.expect("partial timed out").expect("closed");
        assert!(
            matches!(ev, TranscriptEvent::Partial(ref t) if t.starts_with("len=")),
            "got {ev:?}"
        );

        driver.finish();

        let mut saw_final = false;
        for _ in 0..5 {
            match timeout(RECV_TIMEOUT, rx.recv()).await {
                Ok(Some(TranscriptEvent::Final(_))) => {
                    saw_final = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(saw_final, "expected a Final event");
    }

    #[tokio::test]
    async fn finish_emits_final_below_partial_threshold() {
        let calls = Arc::new(AtomicUsize::new(0));
        let driver = LocalEngineDriver::new(Arc::new(CountModel { calls }));
        let mut rx = driver.subscribe();

        driver.process_audio(&[0i16; 1000]).unwrap();
        driver.finish();

        let ev = timeout(RECV_TIMEOUT, rx.recv()).await.expect("final timed out").expect("closed");
        assert!(matches!(ev, TranscriptEvent::Final(ref t) if t == "len=1000"), "got {ev:?}");
    }

    #[tokio::test]
    async fn transcription_error_surfaces() {
        let driver = LocalEngineDriver::new(Arc::new(FailModel));
        let mut rx = driver.subscribe();

        driver.finish();
        let ev = timeout(RECV_TIMEOUT, rx.recv()).await.expect("error timed out").expect("closed");
        assert!(matches!(ev, TranscriptEvent::Error(_)), "got {ev:?}");
    }

    #[tokio::test]
    async fn model_panic_surfaces_as_error_and_worker_survives() {
        let driver =
            LocalEngineDriver::new(Arc::new(PanicOnceModel { calls: AtomicUsize::new(0) }));
        let mut rx = driver.subscribe();

        driver.process_audio(&[0i16; 100]).unwrap();
        driver.finish();
        let ev = timeout(RECV_TIMEOUT, rx.recv()).await.expect("error timed out").expect("closed");
        assert!(matches!(ev, TranscriptEvent::Error(_)), "got {ev:?}");

        driver.reset();
        let mut rx = driver.subscribe();
        driver.process_audio(&[0i16; 100]).unwrap();
        driver.finish();
        let ev = timeout(RECV_TIMEOUT, rx.recv()).await.expect("final timed out").expect("closed");
        assert!(matches!(ev, TranscriptEvent::Final(ref t) if t == "len=100"), "got {ev:?}");
    }

    #[test]
    fn reset_clears_buffer() {
        let calls = Arc::new(AtomicUsize::new(0));
        let driver = LocalEngineDriver::new(Arc::new(CountModel { calls }));
        driver.process_audio(&[1i16; 100]).unwrap();
        assert_eq!(driver.get_audio_buffer().len(), 100);
        driver.reset();
        assert!(driver.get_audio_buffer().is_empty());
    }
}
