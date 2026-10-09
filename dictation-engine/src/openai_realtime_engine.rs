//! Hosted OpenAI realtime transcription engine (streaming).
//!
//! Selected with a realtime model spec such as `openai:gpt-live-transcribe`.
//! Each utterance opens one WebSocket transcription session:
//!
//! 1. The first captured audio opens the connection and sends `session.update`
//!    (model, `keywords`, `prompt`, `languages`, `delay`, no server VAD).
//! 2. Audio is resampled to 24 kHz PCM16, batched into ~100 ms frames, and
//!    sent as `input_audio_buffer.append`.
//! 3. Transcript deltas are accumulated and pushed as
//!    [`TranscriptEvent::Partial`], so the overlay shows text while you speak.
//! 4. `finish()` sends `input_audio_buffer.commit`; the `completed` event
//!    becomes [`TranscriptEvent::Final`].
//!
//! **Fallback.** Every captured sample is also kept locally. If the stream
//! fails at any point (connect, protocol error, server error, timeout), the
//! engine waits for `finish()` and then transcribes the full buffer once with
//! the batch [`FALLBACK_MODEL`], so the utterance is never lost. The fallback
//! stays inside the same provider and uses the same key: no cross-engine
//! switching.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::openai_engine::{self, OpenAiOptions};
use crate::post_processing::Stage;
use crate::stream_engine::{EngineError, EventStream, StreamingEngine, TranscriptEvent};

/// Sample rate the realtime session is configured for.
const REALTIME_RATE: u32 = 24_000;
/// Send audio in frames of about 100 ms.
const FRAME_SAMPLES: usize = (REALTIME_RATE / 10) as usize;
/// Batch model used when the stream fails.
pub const FALLBACK_MODEL: &str = "gpt-transcribe";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for `completed` after the commit before falling back.
const COMPLETE_TIMEOUT: Duration = Duration::from_secs(10);

/// Model names served by the realtime engine rather than batch upload.
pub fn is_realtime_model(model: &str) -> bool {
    model.starts_with("gpt-live-transcribe") || model.contains("realtime")
}

type EventSlot = Arc<Mutex<Option<mpsc::UnboundedSender<TranscriptEvent>>>>;

enum SessionMsg {
    Audio(Vec<i16>),
    Commit,
}

struct Session {
    tx: mpsc::UnboundedSender<SessionMsg>,
    task: JoinHandle<()>,
}

pub struct OpenAiRealtimeEngine {
    api_key: String,
    model: String,
    sample_rate: u32,
    options: OpenAiOptions,
    buffer: Arc<Mutex<Vec<i16>>>,
    event_tx: EventSlot,
    session: Mutex<Option<Session>>,
    commit_requested: Arc<AtomicBool>,
    client: reqwest::Client,
}

impl OpenAiRealtimeEngine {
    pub fn new(model: String, sample_rate: u32, options: OpenAiOptions) -> Result<Self> {
        Ok(Self::with_key(openai_engine::api_key_from_env()?, model, sample_rate, options))
    }

    fn with_key(api_key: String, model: String, sample_rate: u32, options: OpenAiOptions) -> Self {
        Self {
            api_key,
            model,
            sample_rate,
            options,
            buffer: Arc::new(Mutex::new(Vec::new())),
            event_tx: Arc::new(Mutex::new(None)),
            session: Mutex::new(None),
            commit_requested: Arc::new(AtomicBool::new(false)),
            client: reqwest::Client::new(),
        }
    }

    /// Start the per-utterance session task if none is running.
    fn ensure_session(&self) -> Option<mpsc::UnboundedSender<SessionMsg>> {
        let mut slot = self.session.lock().ok()?;
        if let Some(s) = slot.as_ref() {
            return Some(s.tx.clone());
        }
        let handle = tokio::runtime::Handle::try_current().ok()?;
        let (tx, rx) = mpsc::unbounded_channel();
        let ctx = SessionCtx {
            api_key: self.api_key.clone(),
            model: self.model.clone(),
            input_rate: self.sample_rate,
            options: self.options.clone(),
            buffer: Arc::clone(&self.buffer),
            event_tx: Arc::clone(&self.event_tx),
            commit_requested: Arc::clone(&self.commit_requested),
            client: self.client.clone(),
        };
        let task = handle.spawn(session_task(ctx, rx));
        *slot = Some(Session { tx: tx.clone(), task });
        Some(tx)
    }
}

impl StreamingEngine for OpenAiRealtimeEngine {
    fn process_audio(&self, samples: &[i16]) -> Result<()> {
        self.buffer
            .lock()
            .map_err(|e| anyhow!("audio buffer lock poisoned: {e}"))?
            .extend_from_slice(samples);
        if let Some(tx) = self.ensure_session() {
            let _ = tx.send(SessionMsg::Audio(samples.to_vec()));
        }
        Ok(())
    }

    fn subscribe(&self) -> EventStream {
        let (tx, rx) = mpsc::unbounded_channel();
        if let Ok(mut slot) = self.event_tx.lock() {
            *slot = Some(tx);
        }
        rx
    }

    fn finish(&self) {
        self.commit_requested.store(true, Ordering::SeqCst);
        match self.ensure_session() {
            Some(tx) => {
                let _ = tx.send(SessionMsg::Commit);
            }
            None => warn!("realtime: no tokio runtime at finish(); no transcript produced"),
        }
    }

    fn reset(&self) {
        if let Ok(mut slot) = self.session.lock() {
            if let Some(s) = slot.take() {
                s.task.abort();
            }
        }
        if let Ok(mut buf) = self.buffer.lock() {
            buf.clear();
        }
        if let Ok(mut slot) = self.event_tx.lock() {
            *slot = None;
        }
        self.commit_requested.store(false, Ordering::SeqCst);
    }

    fn get_audio_buffer(&self) -> Vec<i16> {
        self.buffer.lock().map(|b| b.clone()).unwrap_or_default()
    }

    fn default_stages(&self) -> Vec<Stage> {
        Vec::new()
    }
}

impl Drop for OpenAiRealtimeEngine {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.session.lock() {
            if let Some(s) = slot.take() {
                s.task.abort();
            }
        }
    }
}

struct SessionCtx {
    api_key: String,
    model: String,
    input_rate: u32,
    options: OpenAiOptions,
    buffer: Arc<Mutex<Vec<i16>>>,
    event_tx: EventSlot,
    commit_requested: Arc<AtomicBool>,
    client: reqwest::Client,
}

fn emit(slot: &EventSlot, event: TranscriptEvent) {
    if let Ok(guard) = slot.lock() {
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(event);
        }
    }
}

async fn session_task(ctx: SessionCtx, mut rx: mpsc::UnboundedReceiver<SessionMsg>) {
    let event = match stream_session(&ctx, &mut rx).await {
        Ok(transcript) => TranscriptEvent::Final(transcript),
        Err(e) => {
            warn!("realtime transcription failed ({e:#}); falling back to batch {FALLBACK_MODEL}");
            // Keep consuming until the utterance ends; the buffer holds the audio.
            while !ctx.commit_requested.load(Ordering::SeqCst) {
                match rx.recv().await {
                    Some(SessionMsg::Commit) => break,
                    Some(SessionMsg::Audio(_)) => {}
                    None => return, // reset or drop: nothing to finalize
                }
            }
            let audio = ctx.buffer.lock().map(|b| b.clone()).unwrap_or_default();
            match openai_engine::transcribe(
                &ctx.client,
                &ctx.api_key,
                FALLBACK_MODEL,
                &audio,
                ctx.input_rate,
                &ctx.options,
            )
            .await
            {
                Ok(text) => {
                    info!("realtime fallback produced the final transcript");
                    TranscriptEvent::Final(text)
                }
                Err(e) => TranscriptEvent::Error(EngineError::Backend(format!(
                    "realtime and batch fallback both failed: {e:#}"
                ))),
            }
        }
    };
    emit(&ctx.event_tx, event);
}

/// The session.update payload for a transcription session.
fn session_update(model: &str, options: &OpenAiOptions) -> serde_json::Value {
    let mut transcription = serde_json::json!({ "model": model, "delay": options.delay });
    if openai_engine::supports_prompt(model) {
        if let Some(p) = &options.prompt {
            transcription["prompt"] = p.clone().into();
        }
    }
    if openai_engine::supports_context_lists(model) {
        if !options.keywords.is_empty() {
            transcription["keywords"] = options.keywords.clone().into();
        }
        if !options.languages.is_empty() {
            transcription["languages"] = options.languages.clone().into();
        }
    }
    serde_json::json!({
        "type": "session.update",
        "session": {
            "type": "transcription",
            "audio": {
                "input": {
                    "format": { "type": "audio/pcm", "rate": REALTIME_RATE },
                    "transcription": transcription,
                    "turn_detection": null,
                }
            }
        }
    })
}

fn append_event(pcm: &[i16]) -> String {
    let mut bytes = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    serde_json::json!({
        "type": "input_audio_buffer.append",
        "audio": base64::engine::general_purpose::STANDARD.encode(bytes),
    })
    .to_string()
}

/// Run one streaming session to completion. Returns the final transcript.
async fn stream_session(
    ctx: &SessionCtx,
    rx: &mut mpsc::UnboundedReceiver<SessionMsg>,
) -> Result<String> {
    let mut request = ctx.options.realtime_url.as_str().into_client_request()?;
    request.headers_mut().insert("Authorization", format!("Bearer {}", ctx.api_key).parse()?);

    let (ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request))
        .await
        .context("realtime connect timed out")?
        .context("realtime connect failed")?;
    let (mut sink, mut stream) = ws.split();
    sink.send(Message::Text(session_update(&ctx.model, &ctx.options).to_string().into())).await?;
    debug!("realtime: session opened ({})", ctx.model);

    let mut resampler = Resampler::new(ctx.input_rate, REALTIME_RATE);
    let mut pending: Vec<i16> = Vec::with_capacity(FRAME_SAMPLES * 2);
    let mut transcript = String::new();
    let mut committed = false;
    let complete_deadline = tokio::time::sleep(Duration::from_secs(3600));
    tokio::pin!(complete_deadline);

    loop {
        tokio::select! {
            msg = rx.recv(), if !committed => match msg {
                Some(SessionMsg::Audio(samples)) => {
                    pending.extend(resampler.push(&samples));
                    if pending.len() >= FRAME_SAMPLES {
                        sink.send(Message::Text(append_event(&pending).into())).await?;
                        pending.clear();
                    }
                }
                Some(SessionMsg::Commit) => {
                    if !pending.is_empty() {
                        sink.send(Message::Text(append_event(&pending).into())).await?;
                        pending.clear();
                    }
                    sink.send(Message::Text(
                        serde_json::json!({ "type": "input_audio_buffer.commit" }).to_string().into(),
                    ))
                    .await?;
                    committed = true;
                    complete_deadline
                        .as_mut()
                        .reset(tokio::time::Instant::now() + COMPLETE_TIMEOUT);
                }
                None => bail!("session channel closed"),
            },
            frame = stream.next() => {
                let frame = match frame {
                    Some(f) => f?,
                    None => bail!("server closed the connection"),
                };
                let text = match frame {
                    Message::Text(t) => t.to_string(),
                    Message::Close(c) => bail!("server closed the connection: {c:?}"),
                    _ => continue,
                };
                let event: serde_json::Value = serde_json::from_str(&text)
                    .with_context(|| format!("invalid server event: {text}"))?;
                match event["type"].as_str().unwrap_or_default() {
                    "conversation.item.input_audio_transcription.delta" => {
                        if let Some(d) = event["delta"].as_str() {
                            transcript.push_str(d);
                            emit(&ctx.event_tx, TranscriptEvent::Partial(transcript.trim().to_string()));
                        }
                    }
                    "conversation.item.input_audio_transcription.completed" => {
                        let done = event["transcript"].as_str().unwrap_or(&transcript).trim().to_string();
                        if committed {
                            let _ = sink.send(Message::Close(None)).await;
                            return Ok(done);
                        }
                        transcript = done;
                    }
                    "error" => bail!("server error: {}", event["error"]),
                    other => debug!("realtime: event {other}"),
                }
            }
            _ = &mut complete_deadline, if committed => {
                bail!("no completed transcript within {:?} of commit", COMPLETE_TIMEOUT)
            }
        }
    }
}

/// Streaming linear-interpolation resampler for mono PCM16. Continuous across
/// `push` calls; upsampling speech from 16 kHz to 24 kHz needs nothing fancier.
struct Resampler {
    step: f64,
    /// Absolute input index of `history[0]`.
    offset: u64,
    history: Vec<i16>,
    /// Absolute output index of the next sample to produce.
    next_out: u64,
}

impl Resampler {
    fn new(in_rate: u32, out_rate: u32) -> Self {
        Self { step: in_rate as f64 / out_rate as f64, offset: 0, history: Vec::new(), next_out: 0 }
    }

    fn push(&mut self, input: &[i16]) -> Vec<i16> {
        if (self.step - 1.0).abs() < f64::EPSILON {
            return input.to_vec();
        }
        self.history.extend_from_slice(input);
        let end = self.offset + self.history.len() as u64;
        let mut out = Vec::with_capacity((input.len() as f64 / self.step) as usize + 2);
        loop {
            let pos = self.next_out as f64 * self.step;
            let i = pos.floor() as u64;
            if i + 1 >= end {
                break;
            }
            let frac = pos - i as f64;
            let a = self.history[(i - self.offset) as usize] as f64;
            let b = self.history[(i + 1 - self.offset) as usize] as f64;
            out.push((a + (b - a) * frac).round() as i16);
            self.next_out += 1;
        }
        // Drop input that no future output sample can reference.
        let keep_from = (self.next_out as f64 * self.step).floor() as u64;
        if keep_from > self.offset {
            let drop = (keep_from - self.offset) as usize;
            self.history.drain(..drop.min(self.history.len()));
            self.offset += drop as u64;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// tokio-tungstenite builds its TLS config with `ClientConfig::builder()`,
    /// which panics unless rustls can pick a single crypto provider from its
    /// features. This fails the build instead of the first wss:// connect.
    #[test]
    fn rustls_has_a_default_crypto_provider() {
        let _ = rustls::ClientConfig::builder();
    }

    #[test]
    fn routes_realtime_models() {
        assert!(is_realtime_model("gpt-live-transcribe"));
        assert!(is_realtime_model("gpt-realtime-whisper"));
        assert!(!is_realtime_model("gpt-transcribe"));
        assert!(!is_realtime_model("whisper-1"));
    }

    #[test]
    fn resampler_produces_three_halves_and_is_chunk_invariant() {
        let input: Vec<i16> = (0..16_000).map(|i| ((i % 200) * 100) as i16).collect();
        let mut whole = Resampler::new(16_000, 24_000);
        let a = whole.push(&input);
        let mut chunked = Resampler::new(16_000, 24_000);
        let b: Vec<i16> = input.chunks(137).flat_map(|c| chunked.push(c)).collect();
        assert_eq!(a, b);
        assert!((a.len() as i64 - 24_000).abs() <= 2, "got {}", a.len());
        // Every third output sample lands exactly on an input sample.
        assert_eq!(a[0], input[0]);
        assert_eq!(a[3], input[2]);
        assert_eq!(a[300], input[200]);
    }

    #[test]
    fn session_update_carries_context_for_live_model() {
        let opts = OpenAiOptions {
            prompt: Some("platform engineering".into()),
            keywords: vec!["WorkOS".into()],
            languages: vec!["en".into()],
            delay: "low".into(),
            ..OpenAiOptions::default()
        };
        let v = session_update("gpt-live-transcribe", &opts);
        let t = &v["session"]["audio"]["input"]["transcription"];
        assert_eq!(t["model"], "gpt-live-transcribe");
        assert_eq!(t["keywords"][0], "WorkOS");
        assert_eq!(t["languages"][0], "en");
        assert_eq!(t["delay"], "low");
        assert_eq!(v["session"]["audio"]["input"]["format"]["rate"], 24_000);
        assert!(v["session"]["audio"]["input"]["turn_detection"].is_null());
    }

    /// A local mock of the realtime server: checks the protocol order, sends
    /// deltas and a completed event, and the engine emits partials then Final.
    #[tokio::test]
    async fn streams_partials_then_final_against_mock_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let first: serde_json::Value =
                serde_json::from_str(&ws.next().await.unwrap().unwrap().into_text().unwrap())
                    .unwrap();
            assert_eq!(first["type"], "session.update");
            let mut appended = 0usize;
            loop {
                let m = ws.next().await.unwrap().unwrap();
                let v: serde_json::Value = serde_json::from_str(&m.into_text().unwrap()).unwrap();
                match v["type"].as_str().unwrap() {
                    "input_audio_buffer.append" => {
                        appended += 1;
                        if appended == 1 {
                            for d in ["Hello", " world"] {
                                let ev = serde_json::json!({"type": "conversation.item.input_audio_transcription.delta", "delta": d});
                                ws.send(Message::Text(ev.to_string().into())).await.unwrap();
                            }
                        }
                    }
                    "input_audio_buffer.commit" => {
                        let ev = serde_json::json!({"type": "conversation.item.input_audio_transcription.completed", "transcript": "Hello, world."});
                        ws.send(Message::Text(ev.to_string().into())).await.unwrap();
                        break;
                    }
                    other => panic!("unexpected client event {other}"),
                }
            }
            appended
        });

        let opts = OpenAiOptions {
            delay: "low".into(),
            realtime_url: format!("ws://{addr}"),
            ..OpenAiOptions::default()
        };
        let engine = OpenAiRealtimeEngine::with_key(
            "test-key".into(),
            "gpt-live-transcribe".into(),
            16_000,
            opts,
        );
        let mut rx = engine.subscribe();
        for _ in 0..10 {
            engine.process_audio(&[100i16; 1600]).unwrap(); // 10 x 100 ms
        }
        let mut partials = Vec::new();
        while partials.len() < 2 {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap() {
                TranscriptEvent::Partial(p) => partials.push(p),
                other => panic!("expected partial, got {other:?}"),
            }
        }
        assert_eq!(partials, vec!["Hello", "Hello world"]);
        engine.finish();
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap() {
            TranscriptEvent::Final(t) => assert_eq!(t, "Hello, world."),
            other => panic!("expected final, got {other:?}"),
        }
        assert!(server.await.unwrap() >= 9);
    }

    /// Minimal HTTP server that answers one request with a JSON transcript and
    /// reports whether the request carried the expected multipart fields.
    async fn mock_batch_server() -> (String, JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/audio/transcriptions", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let n = tcp.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf);
                if let Some(h) = text.find("\r\n\r\n") {
                    let len = text[..h]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if buf.len() >= h + 4 + len {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let body = r#"{"text":"fallback text"}"#;
            let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}", body.len(), body);
            tcp.write_all(resp.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&buf).to_string()
        });
        (url, handle)
    }

    /// The realtime endpoint is unreachable: the engine must still produce a
    /// Final by uploading the buffered audio to the batch endpoint.
    #[tokio::test]
    async fn falls_back_to_batch_when_stream_fails() {
        let (batch_url, batch) = mock_batch_server().await;
        // Bind then drop a listener so the port refuses connections.
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);
        let opts = OpenAiOptions {
            delay: "low".into(),
            realtime_url: format!("ws://{dead_addr}"),
            keywords: vec!["WorkOS".into()],
            transcriptions_url: Some(batch_url),
            ..OpenAiOptions::default()
        };
        let engine = OpenAiRealtimeEngine::with_key(
            "test-key".into(),
            "gpt-live-transcribe".into(),
            16_000,
            opts,
        );
        let mut rx = engine.subscribe();
        engine.process_audio(&[100i16; 8000]).unwrap();
        engine.finish();
        match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap() {
            TranscriptEvent::Final(t) => assert_eq!(t, "fallback text"),
            other => panic!("expected fallback final, got {other:?}"),
        }
        let request = batch.await.unwrap();
        assert!(request.contains("gpt-transcribe"), "fallback model missing");
        assert!(request.contains("keywords[]"), "keywords missing");
        assert!(request.contains("WorkOS"));
        assert!(request.to_lowercase().contains("authorization: bearer test-key"));
    }

    /// Reset before finish drops the session without emitting anything.
    #[tokio::test]
    async fn reset_discards_the_session() {
        let opts = OpenAiOptions {
            realtime_url: "ws://127.0.0.1:9".into(),
            transcriptions_url: Some("http://127.0.0.1:9/never".into()),
            ..OpenAiOptions::default()
        };
        let engine =
            OpenAiRealtimeEngine::with_key("k".into(), "gpt-live-transcribe".into(), 16_000, opts);
        let mut rx = engine.subscribe();
        engine.process_audio(&[1i16; 1600]).unwrap();
        engine.reset();
        assert!(engine.get_audio_buffer().is_empty());
        // The old subscription is dropped by reset, so the stream ends.
        assert!(tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().is_none());
    }
}
