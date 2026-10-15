//! Event-emitting transcription contract.
//!
//! Transition target: this [`StreamingEngine`] trait is intended to replace the
//! pull-based [`crate::engine::TranscriptionEngine`]. Instead of the main loop
//! polling `get_current_text()` on its own 200ms cadence, an engine *pushes*
//! [`TranscriptEvent`]s on its own cadence and the loop simply consumes them.
//!
//! Local ONNX models are adapted *up* to this push contract via
//! [`LocalEngineDriver`], which owns the re-transcribe cadence on a dedicated
//! **blocking thread** — moving CPU-bound inference off the tokio runtime
//! (the old path ran inference inside a `tokio::spawn`ed task). Hosted engines
//! (e.g. OpenAI) implement [`StreamingEngine`] directly.
//!
//! Building Step 1: this module is additive and unit-tested but not yet wired
//! into the daemon loop (that is Step 2).
//!
//! Incremental finalization: when audio arrives with VAD verdicts
//! ([`StreamingEngine::process_audio_with_vad`]), the driver splits the
//! utterance into segments at pauses. Each closed segment is transcribed once
//! and its text frozen; partials re-transcribe only the open segment, and
//! [`StreamingEngine::finish`] only has the open tail left to transcribe.

use anyhow::Result;
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, error};

use crate::post_processing::{Stage, LOCAL_MODEL_STAGES};

/// ~0.15s @ 16kHz: minimum buffered audio before the first partial transcription.
const MIN_AUDIO_SAMPLES: usize = 2400;
/// ~0.3s @ 16kHz: new audio required since the last partial before re-transcribing.
const RETRANSCRIBE_THRESHOLD: usize = 4800;
/// Wake cadence for the driver worker (matches the old preview-task poll interval).
const PARTIAL_TICK: Duration = Duration::from_millis(200);
/// Silence after speech that closes a segment.
const SEGMENT_PAUSE_MS: u32 = 500;
/// Once the open segment holds this much audio before its last speech, the
/// shorter pause below closes it, so a speaker who rarely stops still gets
/// segments well under the model's 30 s chunk size.
const LONG_SEGMENT_MS: u32 = 20_000;
/// Pause that closes a segment longer than [`LONG_SEGMENT_MS`].
const LONG_SEGMENT_PAUSE_MS: u32 = 250;

/// A transcription update pushed by an engine on its own cadence.
#[derive(Debug, Clone)]
pub enum TranscriptEvent {
    /// Rolling preview. Replaces any prior partial (not additive).
    Partial(String),
    /// Committed transcript for the utterance.
    Final(String),
    /// Engine-level failure. Surfaced to the consumer; **never** an implicit
    /// signal to switch engines — one engine per session, no fallback.
    Error(EngineError),
}

/// An engine-level failure.
#[derive(Debug, Clone)]
pub enum EngineError {
    /// The model failed to produce a transcription.
    Transcription(String),
    /// A backend/transport failure (network, IO, etc.).
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

/// The event stream handed to the consumer (daemon loop / GUI bridge).
pub type EventStream = tokio_mpsc::UnboundedReceiver<TranscriptEvent>;

/// Push-based transcription engine: feed audio, consume [`TranscriptEvent`]s.
///
/// Both local (via [`LocalEngineDriver`]) and hosted engines satisfy this, so
/// the daemon loop can consume any engine uniformly without branching on kind.
pub trait StreamingEngine: Send + Sync {
    /// Feed captured audio (16-bit PCM, mono, at the configured sample rate).
    fn process_audio(&self, samples: &[i16]) -> Result<()>;

    /// Feed captured audio with the VAD's verdict for it (`None` when the VAD
    /// gave none). Engines that finalize at pauses use the verdict; the
    /// default ignores it.
    fn process_audio_with_vad(&self, samples: &[i16], is_speech: Option<bool>) -> Result<()> {
        let _ = is_speech;
        self.process_audio(samples)
    }

    /// Subscribe to this engine's event stream for the current session.
    /// Calling again replaces the previous subscription.
    fn subscribe(&self) -> EventStream;

    /// Signal end-of-utterance. The engine finalizes and emits [`TranscriptEvent::Final`].
    fn finish(&self);

    /// Clear all state for a new session.
    fn reset(&self);

    /// A copy of the full captured audio buffer (for debug_audio, etc.).
    fn get_audio_buffer(&self) -> Vec<i16>;

    /// The post-processing stages this engine's output needs.
    ///
    /// Every engine must declare this explicitly. Local models need the full
    /// helper chain; hosted models that already return punctuated, cased,
    /// vocabulary-aware text declare fewer or none. The `[pipeline]` config
    /// section can still override the list per provider.
    fn default_stages(&self) -> Vec<Stage>;
}

/// A pure, synchronous local transcription model: samples in, text out.
///
/// The existing ONNX engines implement this by delegating to their internal
/// `transcribe_buffer`. Buffering and cadence are owned by [`LocalEngineDriver`],
/// so implementors stay stateless with respect to the audio buffer.
pub trait LocalModel: Send + Sync {
    fn transcribe(&self, samples: &[i16]) -> Result<String>;
}

/// Control messages from the [`StreamingEngine`] surface to the worker thread.
enum Ctrl {
    /// New audio was buffered — consider emitting a partial.
    Audio,
    /// End of utterance — emit a final over the frozen segments and the open tail.
    Finish,
    /// New session — reset cadence state.
    Reset,
    /// Tear down the worker thread.
    Shutdown,
}

/// Splits the session timeline into segments at pauses, from the VAD verdicts
/// that come with each captured chunk.
///
/// A segment closes once its speech is followed by a long enough pause. The
/// cut sits in the middle of the pause and each side keeps a margin of that
/// pause past the cut: the closed segment runs to `cut + pause / 5` and the
/// next one starts at `cut - pause / 5`. This is the same idea as the chunk
/// overlap in `chunking.rs` (cut where it is quiet, give each side context),
/// but both margins lie inside VAD silence, so no word lands in two segments
/// and the texts join without the timestamp de-duplication that fixed 30 s
/// chunk cuts need.
#[derive(Debug)]
struct Segmenter {
    sample_rate: u32,
    /// Samples observed so far; equals the audio buffer length.
    observed: usize,
    /// Start of the open segment, margin included.
    open_start: usize,
    /// End of the last speech chunk in the open segment, if it has speech.
    last_speech_end: Option<usize>,
    /// Closed segments in timeline order, margins included.
    closed: Vec<Range<usize>>,
}

impl Segmenter {
    fn new(sample_rate: u32) -> Self {
        Self { sample_rate, observed: 0, open_start: 0, last_speech_end: None, closed: Vec::new() }
    }

    fn samples(&self, ms: u32) -> usize {
        (self.sample_rate as u64 * ms as u64 / 1000) as usize
    }

    /// Record `len` samples and the VAD's verdict for them. Without a verdict
    /// nothing closes, so audio fed without a VAD stays one open segment.
    fn observe(&mut self, len: usize, is_speech: Option<bool>) {
        self.observed += len;
        let end = self.observed;
        let Some(speech_end) = self.last_speech_end else {
            if is_speech == Some(true) {
                self.last_speech_end = Some(end);
            }
            return;
        };
        if is_speech != Some(false) {
            // Speech, or no verdict: never cut inside it.
            self.last_speech_end = Some(end);
            return;
        }
        let pause = if speech_end - self.open_start >= self.samples(LONG_SEGMENT_MS) {
            self.samples(LONG_SEGMENT_PAUSE_MS)
        } else {
            self.samples(SEGMENT_PAUSE_MS)
        };
        if end - speech_end < pause {
            return;
        }
        let cut = speech_end + pause / 2;
        let margin = pause / 5;
        self.closed.push(self.open_start..cut + margin);
        self.open_start = cut - margin;
        self.last_speech_end = None;
    }

    /// Whether the open segment needs transcribing: it has speech, or the VAD
    /// never closed a segment (no VAD, or it heard no pause), in which case
    /// the open segment is the whole session and is transcribed as before.
    fn open_needs_transcription(&self) -> bool {
        self.closed.is_empty() || self.last_speech_end.is_some()
    }
}

/// Session state shared between the [`StreamingEngine`] surface and the worker.
struct Shared {
    audio: Vec<i16>,
    segments: Segmenter,
    /// Bumped on reset so the worker drops text frozen in an earlier session.
    session: u64,
}

/// Adapts a synchronous [`LocalModel`] to the push-based [`StreamingEngine`]
/// contract. Owns the audio buffer and runs the throttled re-transcribe loop on
/// a dedicated blocking thread (never on the tokio runtime).
pub struct LocalEngineDriver {
    shared: Arc<Mutex<Shared>>,
    event_tx: Arc<Mutex<Option<tokio_mpsc::UnboundedSender<TranscriptEvent>>>>,
    ctrl_tx: Sender<Ctrl>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl LocalEngineDriver {
    pub fn new(model: Arc<dyn LocalModel>, sample_rate: u32) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            audio: Vec::new(),
            segments: Segmenter::new(sample_rate),
            session: 0,
        }));
        let event_tx: Arc<Mutex<Option<tokio_mpsc::UnboundedSender<TranscriptEvent>>>> =
            Arc::new(Mutex::new(None));
        let (ctrl_tx, ctrl_rx) = unbounded();

        let worker = {
            let shared = Arc::clone(&shared);
            let event_tx = Arc::clone(&event_tx);
            std::thread::Builder::new()
                .name("local-engine-driver".into())
                .spawn(move || run_worker(model, shared, event_tx, ctrl_rx))
                .expect("failed to spawn LocalEngineDriver worker thread")
        };

        Self { shared, event_tx, ctrl_tx, worker: Mutex::new(Some(worker)) }
    }
}

impl StreamingEngine for LocalEngineDriver {
    fn process_audio(&self, samples: &[i16]) -> Result<()> {
        self.process_audio_with_vad(samples, None)
    }

    fn process_audio_with_vad(&self, samples: &[i16], is_speech: Option<bool>) -> Result<()> {
        {
            let mut shared = self
                .shared
                .lock()
                .map_err(|e| anyhow::anyhow!("audio buffer lock poisoned: {e}"))?;
            shared.audio.extend_from_slice(samples);
            shared.segments.observe(samples.len(), is_speech);
        }
        // Worker may have already exited (shutdown); ignore send failure.
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
        if let Ok(mut shared) = self.shared.lock() {
            shared.audio.clear();
            shared.segments = Segmenter::new(shared.segments.sample_rate);
            shared.session += 1;
        }
        if let Ok(mut slot) = self.event_tx.lock() {
            *slot = None;
        }
        let _ = self.ctrl_tx.send(Ctrl::Reset);
    }

    fn get_audio_buffer(&self) -> Vec<i16> {
        self.shared.lock().map(|s| s.audio.clone()).unwrap_or_default()
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

/// Emit an event to the current subscriber, if any.
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

/// What the worker has done in the current session.
#[derive(Default)]
struct Progress {
    session: u64,
    /// Text of each closed segment transcribed so far, in timeline order.
    frozen: Vec<String>,
    /// Buffer length at the last partial.
    last_len: usize,
}

/// Audio the worker copies out of [`Shared`] so inference runs without the lock.
struct Snapshot {
    total: usize,
    /// Closed segments not transcribed yet.
    closed: Vec<Vec<i16>>,
    /// The open segment, or `None` when it holds only silence after a pause.
    open: Option<Vec<i16>>,
}

/// Copy out what the worker needs next, starting over if the session changed.
fn snapshot(shared: &Mutex<Shared>, progress: &mut Progress) -> Option<Snapshot> {
    let Ok(shared) = shared.lock() else {
        error!("driver: audio buffer lock poisoned; stopping worker");
        return None;
    };
    if shared.session != progress.session {
        *progress = Progress { session: shared.session, ..Progress::default() };
    }
    let segments = &shared.segments;
    let closed = segments.closed[progress.frozen.len().min(segments.closed.len())..]
        .iter()
        .map(|r| shared.audio[r.clone()].to_vec())
        .collect();
    let open =
        segments.open_needs_transcription().then(|| shared.audio[segments.open_start..].to_vec());
    Some(Snapshot { total: shared.audio.len(), closed, open })
}

/// Transcribe newly closed segments and freeze their text, returning how many
/// were frozen. On error the failed segment and those after it stay pending
/// and are retried on the next pass.
fn freeze(model: &dyn LocalModel, progress: &mut Progress, closed: &[Vec<i16>]) -> Result<usize> {
    for audio in closed {
        let text = model.transcribe(audio)?;
        debug!(
            "driver: froze segment {} ({} samples) -> {:?}",
            progress.frozen.len(),
            audio.len(),
            text
        );
        progress.frozen.push(text);
    }
    Ok(closed.len())
}

/// Frozen segment texts followed by the open segment's text.
fn join_text(frozen: &[String], open: &str) -> String {
    frozen
        .iter()
        .map(String::as_str)
        .chain([open])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The driver's blocking worker loop. Wakes on new audio or every
/// [`PARTIAL_TICK`]; freezes segments as they close, emits
/// [`TranscriptEvent::Partial`] once enough new audio has accrued, and
/// [`TranscriptEvent::Final`] on [`Ctrl::Finish`].
fn run_worker(
    model: Arc<dyn LocalModel>,
    shared: Arc<Mutex<Shared>>,
    event_tx: Arc<Mutex<Option<tokio_mpsc::UnboundedSender<TranscriptEvent>>>>,
    ctrl_rx: Receiver<Ctrl>,
) {
    let mut progress = Progress::default();
    let transcription_error =
        |e: anyhow::Error| TranscriptEvent::Error(EngineError::Transcription(e.to_string()));

    loop {
        match ctrl_rx.recv_timeout(PARTIAL_TICK) {
            Ok(Ctrl::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,

            Ok(Ctrl::Reset) => progress = Progress::default(),

            Ok(Ctrl::Finish) => {
                let Some(snap) = snapshot(&shared, &mut progress) else { break };
                let result = freeze(model.as_ref(), &mut progress, &snap.closed).and_then(|_| {
                    snap.open.as_deref().map_or(Ok(String::new()), |a| model.transcribe(a))
                });
                match result {
                    Ok(open_text) => {
                        let text = join_text(&progress.frozen, &open_text);
                        debug!(
                            "driver: final from {} frozen segments + {} open samples -> {:?}",
                            progress.frozen.len(),
                            snap.open.as_ref().map_or(0, Vec::len),
                            text
                        );
                        emit(&event_tx, TranscriptEvent::Final(text));
                    }
                    Err(e) => emit(&event_tx, transcription_error(e)),
                }
                progress.last_len = 0;
            }

            Ok(Ctrl::Audio) | Err(RecvTimeoutError::Timeout) => {
                let Some(snap) = snapshot(&shared, &mut progress) else { break };
                let frozen_now = match freeze(model.as_ref(), &mut progress, &snap.closed) {
                    Ok(n) => n,
                    Err(e) => {
                        emit(&event_tx, transcription_error(e));
                        continue;
                    }
                };

                // A newly frozen segment changes the preview even without much new audio.
                let cur = snap.total;
                if cur < MIN_AUDIO_SAMPLES
                    || (frozen_now == 0 && cur <= progress.last_len + RETRANSCRIBE_THRESHOLD)
                {
                    continue;
                }

                let open_text = match snap.open.as_deref() {
                    Some(audio) if audio.len() >= MIN_AUDIO_SAMPLES => model.transcribe(audio),
                    _ => Ok(String::new()),
                };
                match open_text {
                    Ok(open_text) => {
                        progress.last_len = cur;
                        let text = join_text(&progress.frozen, &open_text);
                        debug!("driver: partial at {} samples -> {:?}", cur, text);
                        emit(&event_tx, TranscriptEvent::Partial(text));
                    }
                    Err(e) => emit(&event_tx, transcription_error(e)),
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

    /// Deterministic model: transcript encodes the sample count it saw.
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

    const RECV_TIMEOUT: TDuration = TDuration::from_secs(3);

    #[tokio::test]
    async fn emits_partial_then_final() {
        let calls = Arc::new(AtomicUsize::new(0));
        let driver = LocalEngineDriver::new(Arc::new(CountModel { calls: calls.clone() }), 16_000);
        let mut rx = driver.subscribe();

        // Enough new audio to clear MIN + RETRANSCRIBE_THRESHOLD → one partial.
        driver.process_audio(&[0i16; MIN_AUDIO_SAMPLES + RETRANSCRIBE_THRESHOLD + 1]).unwrap();

        let ev =
            timeout(RECV_TIMEOUT, rx.recv()).await.expect("partial timed out").expect("closed");
        assert!(
            matches!(ev, TranscriptEvent::Partial(ref t) if t.starts_with("len=")),
            "got {ev:?}"
        );

        driver.finish();

        // The Final may arrive after another partial tick; drain a few events.
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
        let driver = LocalEngineDriver::new(Arc::new(CountModel { calls }), 16_000);
        let mut rx = driver.subscribe();

        // Below MIN_AUDIO_SAMPLES → no partial, but finish must still finalize.
        driver.process_audio(&[0i16; 1000]).unwrap();
        driver.finish();

        let ev = timeout(RECV_TIMEOUT, rx.recv()).await.expect("final timed out").expect("closed");
        assert!(matches!(ev, TranscriptEvent::Final(ref t) if t == "len=1000"), "got {ev:?}");
    }

    #[tokio::test]
    async fn transcription_error_surfaces() {
        let driver = LocalEngineDriver::new(Arc::new(FailModel), 16_000);
        let mut rx = driver.subscribe();

        driver.finish(); // triggers a final transcribe → error
        let ev = timeout(RECV_TIMEOUT, rx.recv()).await.expect("error timed out").expect("closed");
        assert!(matches!(ev, TranscriptEvent::Error(_)), "got {ev:?}");
    }

    #[test]
    fn reset_clears_buffer() {
        let calls = Arc::new(AtomicUsize::new(0));
        let driver = LocalEngineDriver::new(Arc::new(CountModel { calls }), 16_000);
        driver.process_audio(&[1i16; 100]).unwrap();
        assert_eq!(driver.get_audio_buffer().len(), 100);
        driver.reset();
        assert!(driver.get_audio_buffer().is_empty());
    }

    /// Records the length of every slice it transcribes; text is `len=N`.
    #[derive(Default)]
    struct RecordModel {
        lens: Mutex<Vec<usize>>,
    }
    impl LocalModel for RecordModel {
        fn transcribe(&self, samples: &[i16]) -> Result<String> {
            self.lens.lock().unwrap().push(samples.len());
            Ok(format!("len={}", samples.len()))
        }
    }

    const SEC: usize = 16_000;

    /// Drain events until the Final, returning (partials, final text).
    async fn collect_until_final(rx: &mut EventStream) -> (Vec<String>, String) {
        let mut partials = Vec::new();
        loop {
            match timeout(RECV_TIMEOUT, rx.recv()).await.expect("final timed out") {
                Some(TranscriptEvent::Partial(t)) => partials.push(t),
                Some(TranscriptEvent::Final(t)) => return (partials, t),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn segmenter_cuts_mid_pause_with_margins() {
        let mut seg = Segmenter::new(16_000);
        seg.observe(2 * SEC, Some(true));
        seg.observe(SEC / 4, Some(false)); // 250 ms: too short
        assert!(seg.closed.is_empty());
        seg.observe(SEC / 4, Some(false)); // 500 ms pause reached
                                           // pause 8000 samples: cut at 32000 + 4000, margin 1600 each side.
        assert_eq!(seg.closed, vec![0..37_600]);
        assert_eq!(seg.open_start, 34_400);
        assert!(!seg.open_needs_transcription(), "silence after a pause");
        seg.observe(SEC, Some(true));
        assert!(seg.open_needs_transcription());
    }

    #[test]
    fn segmenter_needs_a_verdict_to_cut() {
        let mut seg = Segmenter::new(16_000);
        seg.observe(SEC, None);
        seg.observe(5 * SEC, None);
        assert!(seg.closed.is_empty());
        assert!(seg.open_needs_transcription());

        // Unknown verdicts after speech extend it rather than count as pause.
        seg.observe(SEC, Some(true));
        seg.observe(SEC, None);
        seg.observe(SEC / 4, Some(false));
        assert!(seg.closed.is_empty());
    }

    #[test]
    fn segmenter_ignores_silence_without_speech() {
        let mut seg = Segmenter::new(16_000);
        seg.observe(10 * SEC, Some(false));
        assert!(seg.closed.is_empty());
        assert!(seg.open_needs_transcription(), "nothing closed yet: transcribe as before");
    }

    #[test]
    fn segmenter_long_segment_closes_on_shorter_pause() {
        let mut seg = Segmenter::new(16_000);
        seg.observe(21 * SEC, Some(true));
        seg.observe(SEC / 4, Some(false)); // 250 ms closes a >20 s segment
        assert_eq!(seg.closed.len(), 1);
        // pause 4000 samples: cut at 336000 + 2000, margin 800.
        assert_eq!(seg.closed[0], 0..338_800);
        assert_eq!(seg.open_start, 337_200);
    }

    #[tokio::test]
    async fn closed_segment_is_transcribed_once_and_final_covers_only_the_tail() {
        let model = Arc::new(RecordModel::default());
        let driver = LocalEngineDriver::new(model.clone(), 16_000);
        let mut rx = driver.subscribe();

        driver.process_audio_with_vad(&[1i16; 2 * SEC], Some(true)).unwrap();
        driver.process_audio_with_vad(&[0i16; SEC], Some(false)).unwrap();
        driver.process_audio_with_vad(&[1i16; SEC / 2], Some(true)).unwrap();
        driver.finish();

        let (partials, final_text) = collect_until_final(&mut rx).await;
        // Segment 0..37600 closed; the tail runs from 34400 to 56000.
        assert_eq!(final_text, "len=37600 len=21600");
        assert!(
            partials.iter().any(|p| p.starts_with("len=37600")),
            "a partial shows the frozen segment: {partials:?}"
        );
        let lens = model.lens.lock().unwrap();
        assert_eq!(lens.iter().filter(|&&n| n == 37_600).count(), 1, "calls: {lens:?}");
        assert!(!lens.contains(&56_000), "never re-transcribes the whole buffer: {lens:?}");
    }

    #[tokio::test]
    async fn silent_tail_after_a_pause_is_not_transcribed() {
        let model = Arc::new(RecordModel::default());
        let driver = LocalEngineDriver::new(model.clone(), 16_000);
        let mut rx = driver.subscribe();

        driver.process_audio_with_vad(&[1i16; 2 * SEC], Some(true)).unwrap();
        driver.process_audio_with_vad(&[0i16; SEC], Some(false)).unwrap();
        driver.finish();

        let (_, final_text) = collect_until_final(&mut rx).await;
        assert_eq!(final_text, "len=37600");
        let lens = model.lens.lock().unwrap();
        assert_eq!(lens.iter().filter(|&&n| n == 37_600).count(), 1, "calls: {lens:?}");
        assert!(!lens.contains(&13_600), "silent tail skipped: {lens:?}");
    }

    #[tokio::test]
    async fn reset_drops_frozen_segments() {
        let model = Arc::new(RecordModel::default());
        let driver = LocalEngineDriver::new(model, 16_000);

        let mut rx = driver.subscribe();
        driver.process_audio_with_vad(&[1i16; 2 * SEC], Some(true)).unwrap();
        driver.process_audio_with_vad(&[0i16; SEC], Some(false)).unwrap();
        driver.finish();
        assert_eq!(collect_until_final(&mut rx).await.1, "len=37600");

        driver.reset();
        let mut rx = driver.subscribe();
        driver.process_audio(&[1i16; 1000]).unwrap();
        driver.finish();
        assert_eq!(collect_until_final(&mut rx).await.1, "len=1000");
    }
}
