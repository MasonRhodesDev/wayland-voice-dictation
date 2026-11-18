use anyhow::Result;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use serde::Deserialize;
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tracing::{debug, error, info, warn};

mod app_profile;
pub mod audio_backend;
mod chunking;
pub mod control;
pub mod control_ipc;
pub mod ctc_direct_engine;
pub mod ctc_engine;
pub mod ctc_features;
pub mod debug_audio;
pub mod engine;
pub mod hotword_trie;
pub mod model_selector;
pub mod openai_engine;
pub mod openai_realtime_engine;
pub mod parakeet_engine;
pub mod platform;
pub mod post_processing;
pub mod stream_engine;
pub mod user_dictionary;
pub mod vad;
mod window_detect;

pub use dictation_types::{GuiControl, GuiState, GuiStatus};

use audio_backend::{AudioBackend, AudioBackendConfig, BackendType};
use control::{ControlContext, DaemonCommand, DaemonState};
use model_selector::{EngineOptions, ModelSpec, Provider};
use openai_engine::OpenAiConfig;
use post_processing::stages::{describe as describe_stages, resolve_stages};
use post_processing::{
    LlmCorrectionConfig, Pipeline, PipelinePass, SanitizationProcessor, Stage, StageContext,
    StageSwitches, TextProcessor, WordSubstitutionProcessor,
};
use stream_engine::{StreamingEngine, TranscriptEvent};
use user_dictionary::UserDictionary;

struct RecordingSession {
    #[allow(dead_code)]
    start_time: Instant,
    engine: Arc<dyn StreamingEngine>,
}

#[derive(Debug, Deserialize)]
struct Config {
    daemon: DaemonConfig,
    #[serde(default)]
    pipeline: PipelineConfig,
    #[serde(default)]
    openai: OpenAiConfig,
    #[serde(default)]
    llm_correction: LlmCorrectionConfig,
}

#[derive(Debug, Default, Deserialize)]
struct PipelineConfig {
    #[serde(default)]
    parakeet: Option<String>,
    #[serde(default)]
    openai: Option<String>,
}

impl PipelineConfig {
    fn for_provider(&self, provider: Provider) -> Option<&str> {
        match provider {
            Provider::Parakeet => self.parakeet.as_deref(),
            Provider::OpenAi => self.openai.as_deref(),
        }
    }
}

impl Config {
    fn stage_switches(&self) -> StageSwitches {
        StageSwitches {
            acronyms: self.daemon.enable_acronyms,
            punctuation: self.daemon.enable_punctuation,
            word_substitution: self.daemon.enable_word_substitution,
            fuzzy_vocab: self.daemon.enable_fuzzy_vocab,
            grammar: self.daemon.enable_grammar,
        }
    }

    fn session_stages(&self, spec: &ModelSpec, engine: &dyn StreamingEngine) -> Vec<Stage> {
        resolve_stages(
            &engine.default_stages(),
            self.pipeline.for_provider(spec.provider),
            self.stage_switches(),
        )
    }

    fn engine_options(&self, user_dict: &UserDictionary) -> EngineOptions {
        EngineOptions { openai: self.openai.to_options(&user_dict.app_words()) }
    }
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct DaemonConfig {
    audio_device: String,
    sample_rate: String,

    #[serde(default = "default_model", alias = "preview_model")]
    model: String,

    #[serde(default = "default_enable_acronyms")]
    enable_acronyms: bool,
    #[serde(default = "default_enable_punctuation")]
    enable_punctuation: bool,
    #[serde(default = "default_enable_grammar")]
    enable_grammar: bool,
    #[serde(default = "default_enable_word_substitution")]
    enable_word_substitution: bool,

    #[serde(default = "default_enable_fuzzy_vocab")]
    enable_fuzzy_vocab: bool,

    #[serde(default = "default_silence_threshold_db")]
    silence_threshold_db: f32,
    #[serde(default = "default_debug_audio")]
    debug_audio: bool,

    #[serde(default = "default_trailing_buffer_ms")]
    trailing_buffer_ms: u64,

    #[serde(default = "default_audio_backend")]
    audio_backend: String,

    #[serde(default = "default_idle_release_timeout_secs")]
    idle_release_timeout_secs: u64,

    #[serde(default = "default_media_resume_delay_ms")]
    media_resume_delay_ms: u64,

    #[serde(default = "default_engine_idle_timeout_secs")]
    engine_idle_timeout_secs: u64,

    #[serde(default = "default_enable_correction_learning")]
    enable_correction_learning: bool,

    #[serde(default = "default_correction_monitor_duration_secs")]
    correction_monitor_duration_secs: u64,

    #[serde(default = "default_correction_auto_promote_threshold")]
    correction_auto_promote_threshold: u32,

    #[serde(default = "default_correction_max_age_days")]
    correction_max_age_days: u32,

    #[serde(default = "default_enable_accessibility_bridge")]
    enable_accessibility_bridge: bool,

    #[serde(default = "default_correction_backend_wezterm")]
    correction_backend_wezterm: bool,
}

impl DaemonConfig {
    fn correction_settings(&self) -> platform::CorrectionSettings {
        platform::CorrectionSettings {
            enabled: self.enable_correction_learning,
            monitor_duration_secs: self.correction_monitor_duration_secs,
            auto_promote_threshold: self.correction_auto_promote_threshold,
            max_age_days: self.correction_max_age_days,
            accessibility_bridge: self.enable_accessibility_bridge,
            wezterm_backend: self.correction_backend_wezterm,
        }
    }
}

fn default_model() -> String {
    "parakeet:default".to_string()
}
fn default_enable_acronyms() -> bool {
    true
}
fn default_enable_punctuation() -> bool {
    true
}
fn default_enable_grammar() -> bool {
    true
}
fn default_enable_fuzzy_vocab() -> bool {
    true
}
fn default_enable_word_substitution() -> bool {
    true
}
fn default_silence_threshold_db() -> f32 {
    -60.0
}
fn default_debug_audio() -> bool {
    false
}
fn default_trailing_buffer_ms() -> u64 {
    750
}
fn default_audio_backend() -> String {
    "auto".to_string()
}
fn default_idle_release_timeout_secs() -> u64 {
    30
}
fn default_media_resume_delay_ms() -> u64 {
    25
}
fn default_engine_idle_timeout_secs() -> u64 {
    300
}
fn default_enable_correction_learning() -> bool {
    true
}
fn default_correction_monitor_duration_secs() -> u64 {
    60
}
fn default_correction_auto_promote_threshold() -> u32 {
    3
}
fn default_correction_max_age_days() -> u32 {
    30
}
fn default_enable_accessibility_bridge() -> bool {
    true
}
fn default_correction_backend_wezterm() -> bool {
    true
}

fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

fn load_config() -> Result<Config> {
    let home = std::env::var("HOME")?;
    let config_path = format!("{}/.config/voice-dictation/config.toml", home);

    let config_str = fs::read_to_string(&config_path)
        .map_err(|e| anyhow::anyhow!("Failed to read config file {}: {}", config_path, e))?;

    let config: Config = toml::from_str(&config_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse config: {}", e))?;

    Ok(config)
}

async fn watch_dictionary_files(user_dict: Arc<UserDictionary>) -> Result<()> {
    let paths = user_dict.watch_paths();

    if paths.is_empty() {
        info!("No dictionary files to watch");
        return Ok(());
    }

    info!("Watching dictionary files: {:?}", paths);

    let (tx, mut rx) = mpsc::channel(100);

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res {
            let _ = tx.blocking_send(event);
        }
    })?;

    for path in &paths {
        if path.exists() {
            watcher.watch(path, RecursiveMode::NonRecursive)?;
        } else if let Some(parent) = path.parent() {
            if parent.exists() {
                watcher.watch(parent, RecursiveMode::NonRecursive)?;
            }
        }
    }

    while let Some(event) = rx.recv().await {
        match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
                for path in &paths {
                    if event.paths.iter().any(|p| p == path) {
                        info!("Dictionary file changed: {:?}, reloading...", path);
                        if let Err(e) = user_dict.reload_all() {
                            warn!("Failed to reload dictionaries: {}", e);
                        } else {
                            info!("Dictionaries reloaded successfully");
                        }
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    Ok(())
}

async fn watch_substitution_file(word_sub: WordSubstitutionProcessor) -> Result<()> {
    let path = WordSubstitutionProcessor::watch_path();

    if path.as_os_str().is_empty() {
        info!("No substitution file path to watch");
        return Ok(());
    }

    info!("Watching substitution file: {:?}", path);

    let (tx, mut rx) = mpsc::channel(100);

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res {
            let _ = tx.blocking_send(event);
        }
    })?;

    if path.exists() {
        watcher.watch(&path, RecursiveMode::NonRecursive)?;
    } else if let Some(parent) = path.parent() {
        if parent.exists() {
            watcher.watch(parent, RecursiveMode::NonRecursive)?;
        }
    }

    while let Some(event) = rx.recv().await {
        match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                if event.paths.iter().any(|p| p == &path) =>
            {
                info!("Substitution file changed: {:?}, reloading...", path);
                if let Err(e) = word_sub.reload() {
                    warn!("Failed to reload substitutions: {}", e);
                } else {
                    info!("Substitutions reloaded successfully");
                }
            }
            _ => {}
        }
    }

    Ok(())
}

pub struct HealthState {
    pub audio_healthy: AtomicBool,
    pub engine_healthy: AtomicBool,
    pub gui_healthy: AtomicBool,
    pub last_audio_timestamp_ms: AtomicU64,
    pub last_error: RwLock<Option<String>>,
}

impl HealthState {
    fn new() -> Self {
        Self {
            audio_healthy: AtomicBool::new(false),
            engine_healthy: AtomicBool::new(false),
            gui_healthy: AtomicBool::new(false),
            last_audio_timestamp_ms: AtomicU64::new(0),
            last_error: RwLock::new(None),
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.engine_healthy.load(Ordering::Relaxed)
    }
}

#[derive(Clone)]
struct DeviceManagerConfig {
    backend_type: BackendType,
    backend_config: AudioBackendConfig,
    idle_release_timeout_secs: u64,
}

struct DeviceManager {
    config: DeviceManagerConfig,
    backend: Option<Box<dyn AudioBackend>>,
    audio_tx: mpsc::UnboundedSender<Vec<i16>>,
    needs_recreate: Arc<std::sync::atomic::AtomicBool>,
    stopped_at: Option<Instant>,
}

impl DeviceManager {
    fn new(config: DeviceManagerConfig, audio_tx: mpsc::UnboundedSender<Vec<i16>>) -> Result<Self> {
        info!("DeviceManager: Pre-creating audio backend ({:?})...", config.backend_type);
        let backend = Self::create_backend(&config, audio_tx.clone())?;

        Ok(Self {
            config,
            backend: Some(backend),
            audio_tx,
            needs_recreate: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            stopped_at: None,
        })
    }

    fn create_backend(
        config: &DeviceManagerConfig,
        tx: mpsc::UnboundedSender<Vec<i16>>,
    ) -> Result<Box<dyn AudioBackend>> {
        audio_backend::create_backend(config.backend_type, tx, &config.backend_config)
    }

    fn start(&mut self) -> Result<()> {
        const MAX_RETRIES: u32 = 3;
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

        if self.stopped_at.take().is_some() {
            debug!("DeviceManager: Cleared idle timer (restarting before timeout)");
        }

        self.needs_recreate.swap(false, std::sync::atomic::Ordering::SeqCst);

        if self.backend.is_none() {
            info!("DeviceManager: Creating audio backend...");

            let mut last_error = None;
            for attempt in 1..=MAX_RETRIES {
                match Self::create_backend(&self.config, self.audio_tx.clone()) {
                    Ok(backend) => {
                        self.backend = Some(backend);
                        info!("DeviceManager: Audio backend created");
                        break;
                    }
                    Err(e) if attempt < MAX_RETRIES => {
                        warn!(
                            "DeviceManager: Backend creation failed (attempt {}): {}, retrying...",
                            attempt, e
                        );
                        last_error = Some(e);
                        std::thread::sleep(RETRY_DELAY);
                    }
                    Err(e) => {
                        last_error = Some(e);
                    }
                }
            }

            if self.backend.is_none() {
                return Err(
                    last_error.unwrap_or_else(|| anyhow::anyhow!("Failed to create audio backend"))
                );
            }
        }

        if let Some(ref backend) = self.backend {
            backend.start()?;
        } else {
            return Err(anyhow::anyhow!("No audio backend available"));
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(ref backend) = self.backend {
            backend.stop()?;

            if backend.releases_on_stop() {
                let timeout_secs = self.config.idle_release_timeout_secs;
                if timeout_secs == 0 {
                    self.backend = None;
                    self.stopped_at = None;
                    info!("DeviceManager: Audio backend released immediately");
                } else {
                    self.stopped_at = Some(Instant::now());
                    info!(
                        "DeviceManager: Audio stopped, will release after {}s idle",
                        timeout_secs
                    );
                }
            } else {
                self.stopped_at = None;
                info!("DeviceManager: Audio stopped (backend kept open for sharing)");
            }
        }
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        if let Some(ref backend) = self.backend {
            backend.flush()?;
        }
        Ok(())
    }

    fn check_idle_timeout(&mut self) -> bool {
        if let Some(stopped_at) = self.stopped_at {
            let idle_duration = stopped_at.elapsed();
            let timeout = Duration::from_secs(self.config.idle_release_timeout_secs);
            if idle_duration >= timeout {
                self.release();
                self.stopped_at = None;
                return true;
            }
        }
        false
    }

    fn release(&mut self) {
        if self.backend.take().is_some() {
            info!("DeviceManager: Audio backend released after idle timeout");
        }
    }

    fn set_device(&mut self, device_name: Option<String>) {
        info!(
            "DeviceManager: Switching device to {:?}",
            device_name.as_deref().unwrap_or("Default")
        );
        self.config.backend_config.device_name = device_name;
        self.backend.take();
        self.stopped_at = None;
    }

    fn spawn_device_watcher(&self) {
        let needs_recreate = Arc::clone(&self.needs_recreate);
        let Some(snd_path) = platform::audio_hotplug_dir() else {
            debug!("DeviceManager: no audio device directory to watch on this platform");
            return;
        };

        std::thread::spawn(move || {
            if !snd_path.exists() {
                warn!(
                    "DeviceManager: {} not found, device hotplug detection disabled",
                    snd_path.display()
                );
                return;
            }

            let flag = needs_recreate;
            let mut watcher = match notify::recommended_watcher(
                move |res: std::result::Result<Event, notify::Error>| {
                    if let Ok(event) = res {
                        match event.kind {
                            EventKind::Create(_) | EventKind::Remove(_) => {
                                info!("DeviceManager: Audio device change detected, will recreate on next start");
                                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                            }
                            _ => {}
                        }
                    }
                },
            ) {
                Ok(w) => w,
                Err(e) => {
                    error!("DeviceManager: Failed to create watcher: {}", e);
                    return;
                }
            };

            if let Err(e) = watcher.watch(snd_path, RecursiveMode::NonRecursive) {
                error!("DeviceManager: Failed to watch {}: {}", snd_path.display(), e);
                return;
            }

            info!("DeviceManager: Watching {} for device changes", snd_path.display());

            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        });
    }
}

#[allow(dead_code)]
async fn drain_audio_channel(
    audio_rx: &Arc<Mutex<mpsc::UnboundedReceiver<Vec<i16>>>>,
    engine: &Arc<dyn StreamingEngine>,
    timeout_ms: u64,
) -> usize {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    let mut drained = 0;

    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }

        match tokio::time::timeout(Duration::from_millis(10), async {
            let mut rx = audio_rx.lock().await;
            rx.recv().await
        })
        .await
        {
            Ok(Some(samples)) => {
                if let Err(e) = engine.process_audio(&samples) {
                    error!("Processing error during drain: {}", e);
                }
                drained += 1;
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    debug!("Drained {} audio chunks from channel", drained);
    drained
}

async fn recv_final_transcript(rx: &mut stream_engine::EventStream) -> String {
    loop {
        match tokio::time::timeout(Duration::from_secs(60), rx.recv()).await {
            Ok(Some(TranscriptEvent::Final(text))) => return text,
            Ok(Some(TranscriptEvent::Partial(_))) => continue,
            Ok(Some(TranscriptEvent::Error(e))) => {
                error!("Final transcription error: {}", e);
                return String::new();
            }
            Ok(None) => {
                error!("Engine event stream closed before Final");
                return String::new();
            }
            Err(_) => {
                error!("Final transcription timed out");
                return String::new();
            }
        }
    }
}

struct ProcessingGuard {
    tx: broadcast::Sender<GuiControl>,
    armed: bool,
}

impl ProcessingGuard {
    fn new(tx: broadcast::Sender<GuiControl>) -> Self {
        Self { tx, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessingGuard {
    fn drop(&mut self) {
        if self.armed {
            warn!("Processing path exited before clearing GUI state; forcing SetHidden");
            let _ = self.tx.send(GuiControl::SetHidden);
        }
    }
}

#[tokio::main]
pub async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    info!("Starting Parakeet dictation engine");

    let config = load_config().unwrap_or_else(|e| {
        warn!("Failed to load config: {}, using defaults", e);
        Config {
            daemon: DaemonConfig {
                audio_device: "default".to_string(),
                sample_rate: "16000".to_string(),
                model: default_model(),
                enable_acronyms: default_enable_acronyms(),
                enable_punctuation: default_enable_punctuation(),
                enable_grammar: default_enable_grammar(),
                enable_word_substitution: default_enable_word_substitution(),
                enable_fuzzy_vocab: default_enable_fuzzy_vocab(),
                silence_threshold_db: default_silence_threshold_db(),
                debug_audio: default_debug_audio(),
                trailing_buffer_ms: default_trailing_buffer_ms(),
                audio_backend: default_audio_backend(),
                idle_release_timeout_secs: default_idle_release_timeout_secs(),
                media_resume_delay_ms: default_media_resume_delay_ms(),
                engine_idle_timeout_secs: default_engine_idle_timeout_secs(),
                enable_correction_learning: default_enable_correction_learning(),
                correction_monitor_duration_secs: default_correction_monitor_duration_secs(),
                correction_auto_promote_threshold: default_correction_auto_promote_threshold(),
                correction_max_age_days: default_correction_max_age_days(),
                enable_accessibility_bridge: default_enable_accessibility_bridge(),
                correction_backend_wezterm: default_correction_backend_wezterm(),
            },
            pipeline: PipelineConfig::default(),
            openai: OpenAiConfig::default(),
            llm_correction: LlmCorrectionConfig::default(),
        }
    });

    let sample_rate: u32 = config.daemon.sample_rate.parse().unwrap_or_else(|_| {
        warn!("Invalid sample_rate '{}', defaulting to 16000", config.daemon.sample_rate);
        16000
    });

    let silence_threshold = db_to_linear(config.daemon.silence_threshold_db);
    info!(
        "Silence threshold: {:.1}dB ({:.6} linear)",
        config.daemon.silence_threshold_db, silence_threshold
    );

    info!(
        "Config loaded: audio_device={}, sample_rate={}",
        config.daemon.audio_device, sample_rate
    );

    let user_dict = Arc::new(UserDictionary::new().unwrap_or_else(|e| {
        warn!("Failed to initialize user dictionary: {}, spell checking will use defaults only", e);
        UserDictionary::empty()
    }));
    info!("User dictionary initialized");

    let user_dict_watcher = Arc::clone(&user_dict);
    tokio::spawn(async move {
        if let Err(e) = watch_dictionary_files(user_dict_watcher).await {
            error!("Dictionary file watcher error: {}", e);
        }
    });

    let word_sub = if config.daemon.enable_word_substitution {
        match WordSubstitutionProcessor::new(Some(Arc::clone(&user_dict))) {
            Ok(ws) => {
                info!("Word substitution processor initialized");
                Some(ws)
            }
            Err(e) => {
                warn!("Failed to initialize word substitution processor: {}, disabled", e);
                None
            }
        }
    } else {
        None
    };

    if let Some(ref ws) = word_sub {
        let ws_watcher = ws.clone();
        tokio::spawn(async move {
            if let Err(e) = watch_substitution_file(ws_watcher).await {
                error!("Substitution file watcher error: {}", e);
            }
        });
    }

    let mut corrections =
        platform::CorrectionLearning::start(&config.daemon.correction_settings()).await;

    let model_spec = ModelSpec::parse(&config.daemon.model)
        .map_err(|e| anyhow::anyhow!("Invalid model '{}': {}", config.daemon.model, e))?;

    info!("Model: {}", model_spec);

    if !model_spec.is_available() {
        return Err(anyhow::anyhow!(
            "Model '{}' not found at {:?}. Check that the model is installed.",
            config.daemon.model,
            model_spec.model_path()
        ));
    }

    let health_state = Arc::new(HealthState::new());

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        loop {
            interval.tick().await;
            if let Err(e) = platform::notify_watchdog() {
                debug!("Failed to send watchdog keepalive: {}", e);
            }
        }
    });

    let (audio_tx, audio_rx) = mpsc::unbounded_channel::<Vec<i16>>();
    let audio_rx_shared = Arc::new(tokio::sync::Mutex::new(audio_rx));

    let (gui_control_tx, _) = broadcast::channel::<GuiControl>(100);
    let (spectrum_tx, _) = broadcast::channel::<Vec<f32>>(50);
    let (gui_status_tx, mut gui_status_rx) = mpsc::channel::<GuiStatus>(10);

    let audio_device_name =
        if config.daemon.audio_device.is_empty() || config.daemon.audio_device == "default" {
            None
        } else {
            Some(config.daemon.audio_device.clone())
        };

    let backend_type = BackendType::from_str(&config.daemon.audio_backend).unwrap_or_else(|| {
        warn!("Unknown audio backend '{}', using auto", config.daemon.audio_backend);
        BackendType::Auto
    });

    info!("Creating DeviceManager with pre-loaded audio backend...");
    let device_manager_config = DeviceManagerConfig {
        backend_type,
        backend_config: AudioBackendConfig {
            device_name: audio_device_name.clone(),
            sample_rate,
            silence_threshold,
        },
        idle_release_timeout_secs: config.daemon.idle_release_timeout_secs,
    };
    let mut device_manager = DeviceManager::new(device_manager_config, audio_tx)?;

    device_manager.spawn_device_watcher();
    info!("Audio streams pre-loaded and ready (fast startup enabled)");

    info!("Spawning integrated GUI...");
    let overlay =
        platform::spawn_overlay(gui_control_tx.clone(), spectrum_tx.clone(), gui_status_tx);

    let gui_init_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut gui_available = false;
    let mut channel_open = overlay.is_some();
    if channel_open {
        info!("Waiting for GUI to initialize...");
    } else {
        debug!("No overlay on this platform");
    }
    while channel_open {
        match tokio::time::timeout_at(gui_init_deadline, gui_status_rx.recv()).await {
            Ok(Some(GuiStatus::Ready)) => {
                info!("GUI ready");
                gui_available = true;
                break;
            }
            Ok(Some(GuiStatus::Error(e))) => {
                warn!("GUI initialization error (may retry): {}", e);
            }
            Ok(Some(GuiStatus::TransitionComplete { .. })) => {}
            Ok(Some(GuiStatus::ShuttingDown)) => {
                warn!("GUI is shutting down during init, continuing without GUI");
                break;
            }
            Ok(None) => {
                warn!("GUI status channel closed, continuing without GUI");
                channel_open = false;
            }
            Err(_) => {
                warn!("GUI not ready within 15 seconds; continuing startup");
                warn!("The GUI keeps retrying in the background and will attach when ready");
                info!("You can still use voice-dictation start/stop/confirm commands normally");
                break;
            }
        }
    }

    health_state.gui_healthy.store(gui_available, Ordering::Relaxed);

    if overlay.is_some() && !gui_available {
        info!("Running without visual overlay until the GUI reports ready");
    }

    if channel_open {
        let gui_health = Arc::clone(&health_state);
        tokio::spawn(async move {
            while let Some(status) = gui_status_rx.recv().await {
                match status {
                    GuiStatus::Ready => {
                        info!("GUI reported ready");
                        gui_health.gui_healthy.store(true, Ordering::Relaxed);
                    }
                    GuiStatus::Error(e) => {
                        warn!("GUI reported error: {}", e);
                        gui_health.gui_healthy.store(false, Ordering::Relaxed);
                    }
                    GuiStatus::ShuttingDown => {
                        info!("GUI shutting down");
                        gui_health.gui_healthy.store(false, Ordering::Relaxed);
                    }
                    GuiStatus::TransitionComplete { .. } => {}
                }
            }
        });
    }

    info!("Pre-loading Parakeet engine (blocking call before D-Bus)...");
    let mut preview_engine: Option<Arc<dyn StreamingEngine>> =
        Some(model_spec.create_streaming_engine(sample_rate, &config.engine_options(&user_dict))?);
    if let Some(engine) = preview_engine.as_ref() {
        info!(
            "Post-processing stages for {}: {}",
            model_spec,
            describe_stages(&config.session_stages(&model_spec, engine.as_ref()))
        );
    }
    let mut engine_stopped_at: Option<Instant> = None;
    info!("Parakeet engine loaded and ready");

    health_state.engine_healthy.store(true, Ordering::Relaxed);

    let (state_tx, state_rx) = tokio::sync::watch::channel(DaemonState::Idle);
    let (command_tx, mut command_rx) = mpsc::channel::<DaemonCommand>(10);

    let _control_server = platform::serve_control(ControlContext {
        commands: command_tx.clone(),
        state: state_rx,
        health: Arc::clone(&health_state),
        missing_permissions: platform::missing_permissions,
    })
    .await?;

    let _tray_handle = platform::spawn_tray(
        state_tx.subscribe(),
        command_tx.clone(),
        backend_type,
        audio_device_name.clone(),
    );

    info!("Daemon initialized - entering idle state (GUI hidden)");

    if let Err(e) = platform::notify_ready() {
        warn!("Failed to notify service manager (Ready): {}", e);
    }

    let mut daemon_state = DaemonState::Idle;
    let mut session: Option<RecordingSession> = None;
    let mut audio_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut preview_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut media_was_playing = false;
    let mut _idle_inhibit: Option<platform::IdleInhibitor> = None;
    let mut focus_target: Option<platform::FocusTarget> = None;
    let mut restart_requested = false;
    let (cancel_tx, _cancel_rx) = tokio::sync::watch::channel(false);

    let _audio_health_timeout = Duration::from_secs(3);

    loop {
        match daemon_state {
            DaemonState::Idle => {
                if device_manager.check_idle_timeout() {
                    debug!("Idle timeout expired, mic released");
                }

                if let Some(stopped_at) = engine_stopped_at {
                    let timeout = Duration::from_secs(config.daemon.engine_idle_timeout_secs);
                    if stopped_at.elapsed() >= timeout && preview_engine.is_some() {
                        info!("Engine idle timeout expired, releasing ORT sessions to free memory");
                        preview_engine = None;
                        engine_stopped_at = None;
                        health_state.engine_healthy.store(false, Ordering::Relaxed);
                    }
                }

                match tokio::time::timeout(Duration::from_millis(100), command_rx.recv()).await {
                    Ok(Some(cmd)) => match cmd {
                        DaemonCommand::StartRecording => {
                            info!("Received StartRecording command");
                            focus_target = platform::FocusTarget::capture().await;
                            if let Some(ref target) = focus_target {
                                info!("Captured focus target: class={}", target.class());
                            }
                            media_was_playing = platform::pause_media_if_playing();

                            _idle_inhibit = match platform::inhibit_idle(
                                "Active voice dictation session",
                            )
                            .await
                            {
                                Ok(i) => Some(i),
                                Err(e) => {
                                    warn!("Failed to acquire idle inhibit: {}", e);
                                    None
                                }
                            };

                            {
                                let mut rx = audio_rx_shared.lock().await;
                                let mut drained = 0;
                                while rx.try_recv().is_ok() {
                                    drained += 1;
                                }
                                if drained > 0 {
                                    info!("Drained {} stale audio chunks from channel", drained);
                                }
                            }

                            device_manager.start()?;
                            info!("Audio capture started (pre-loaded streams)");

                            health_state.audio_healthy.store(true, Ordering::Relaxed);

                            if preview_engine.is_none() {
                                info!("Recreating transcription engine (was released for idle memory savings)...");
                                preview_engine = Some(model_spec.create_streaming_engine(
                                    sample_rate,
                                    &config.engine_options(&user_dict),
                                )?);
                                health_state.engine_healthy.store(true, Ordering::Relaxed);
                                info!("Engine recreated and ready");
                            }
                            engine_stopped_at = None;

                            let engine = preview_engine.as_ref().unwrap();
                            engine.reset();
                            let session_engine = Arc::clone(engine);

                            gui_control_tx.send(GuiControl::SetListening).map_err(|e| {
                                anyhow::anyhow!("Failed to send SetListening: {}", e)
                            })?;

                            session = Some(RecordingSession {
                                start_time: Instant::now(),
                                engine: Arc::clone(&session_engine),
                            });

                            let _ = cancel_tx.send(false);

                            let audio_notify = Arc::new(tokio::sync::Notify::new());

                            let engine_clone = Arc::clone(&session_engine);
                            let spectrum_tx_clone = spectrum_tx.clone();
                            let audio_rx_clone = Arc::clone(&audio_rx_shared);
                            let mut cancel_rx = cancel_tx.subscribe();
                            let trailing_buffer_ms = config.daemon.trailing_buffer_ms;
                            let health_clone = Arc::clone(&health_state);
                            let audio_notify_tx = Arc::clone(&audio_notify);
                            audio_task = Some(tokio::spawn(async move {
                                let mut buffer = Vec::new();
                                let trailing_duration = Duration::from_millis(trailing_buffer_ms);
                                let mut trailing_deadline: Option<tokio::time::Instant> = None;

                                loop {
                                    if let Some(deadline) = trailing_deadline {
                                        if tokio::time::Instant::now() >= deadline {
                                            debug!("Audio task: trailing capture complete");
                                            break;
                                        }
                                    }

                                    tokio::select! {
                                        biased;
                                        _ = cancel_rx.changed() => {
                                            if *cancel_rx.borrow() && trailing_deadline.is_none() {
                                                debug!("Audio task: cancellation received, starting trailing capture");
                                                trailing_deadline = Some(tokio::time::Instant::now() + trailing_duration);
                                            }
                                        }
                                        samples = async {
                                            let mut rx = audio_rx_clone.lock().await;
                                            rx.recv().await
                                        } => {
                                            match samples {
                                                Some(samples) => {
                                                    let now_ms = std::time::SystemTime::now()
                                                        .duration_since(std::time::UNIX_EPOCH)
                                                        .unwrap_or_default()
                                                        .as_millis() as u64;
                                                    health_clone.last_audio_timestamp_ms.store(now_ms, Ordering::Relaxed);
                                                    health_clone.audio_healthy.store(true, Ordering::Relaxed);

                                                    let samples_f32: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();
                                                    buffer.extend_from_slice(&samples_f32);

                                                    while buffer.len() >= 512 {
                                                        let chunk: Vec<f32> = buffer.drain(..512).collect();
                                                        let _ = spectrum_tx_clone.send(chunk);
                                                    }

                                                    if let Err(e) = engine_clone.process_audio(&samples) {
                                                        error!("Processing error: {}", e);
                                                    }
                                                    audio_notify_tx.notify_one();
                                                }
                                                None => break,
                                            }
                                        }
                                        _ = tokio::time::sleep(Duration::from_millis(10)), if trailing_deadline.is_some() => {}
                                    }
                                }
                                debug!("Audio task: exiting gracefully");
                            }));

                            let mut event_rx = session_engine.subscribe();
                            let gui_control_tx_preview = gui_control_tx.clone();
                            let preview_stages =
                                config.session_stages(&model_spec, session_engine.as_ref());
                            let preview_ctx = StageContext {
                                user_dict: Some(Arc::clone(&user_dict)),
                                word_sub: word_sub.clone(),
                                llm: None,
                            };
                            let mut cancel_rx_preview = cancel_tx.subscribe();
                            preview_task = Some(tokio::spawn(async move {
                                let pipeline = Pipeline::from_stages(
                                    &preview_stages,
                                    &preview_ctx,
                                    PipelinePass::Preview,
                                );

                                let mut last_text = String::new();
                                let mut last_text_change = Instant::now();
                                const TEXT_SETTLED_THRESHOLD_MS: u64 = 300;
                                let mut settle_tick =
                                    tokio::time::interval(Duration::from_millis(200));

                                loop {
                                    tokio::select! {
                                        biased;
                                        _ = cancel_rx_preview.changed() => {
                                            if *cancel_rx_preview.borrow() {
                                                debug!("Preview task: cancellation received");
                                                break;
                                            }
                                        }
                                        ev = event_rx.recv() => {
                                            match ev {
                                                Some(TranscriptEvent::Partial(text_raw)) => {
                                                    debug!("preview: received partial ({} chars), forwarding UpdateTranscription", text_raw.len());
                                                    let text_processed = match pipeline.process(&text_raw) {
                                                        Ok(processed) => processed,
                                                        Err(e) => {
                                                            error!("Preview post-processing error: {}", e);
                                                            text_raw.clone()
                                                        }
                                                    };

                                                    if !pipeline.is_empty() && text_raw != text_processed {
                                                        debug!("[Preview] Raw: '{}' -> Processed: '{}'", text_raw, text_processed);
                                                    }

                                                    if text_processed != last_text {
                                                        last_text = text_processed.clone();
                                                        last_text_change = Instant::now();
                                                    }

                                                    let text_settled = last_text_change.elapsed().as_millis() >= TEXT_SETTLED_THRESHOLD_MS as u128;
                                                    let is_speaking = !text_processed.is_empty() && !text_settled;

                                                    let _ = gui_control_tx_preview.send(GuiControl::UpdateTranscription {
                                                        text: text_processed,
                                                        is_final: false,
                                                    });
                                                    let _ = gui_control_tx_preview.send(GuiControl::UpdateVadState {
                                                        is_speaking,
                                                        text_settled,
                                                    });
                                                }
                                                Some(TranscriptEvent::Final(_)) => {}
                                                Some(TranscriptEvent::Error(e)) => {
                                                    error!("Preview: engine error: {}", e);
                                                }
                                                None => {
                                                    debug!("Preview task: event stream closed");
                                                    break;
                                                }
                                            }
                                        }
                                        _ = settle_tick.tick() => {
                                            let text_settled = last_text_change.elapsed().as_millis() >= TEXT_SETTLED_THRESHOLD_MS as u128;
                                            let is_speaking = !last_text.is_empty() && !text_settled;
                                            let _ = gui_control_tx_preview.send(GuiControl::UpdateVadState {
                                                is_speaking,
                                                text_settled,
                                            });
                                        }
                                    }
                                }
                                debug!("Preview task: exiting gracefully");
                            }));

                            daemon_state = DaemonState::Recording;
                            let _ = state_tx.send(daemon_state);
                            info!("Entered Recording state");
                        }
                        DaemonCommand::SwitchDevice(name) => {
                            info!(
                                "Switching audio device to {:?}",
                                name.as_deref().unwrap_or("Default")
                            );
                            device_manager.set_device(name);
                        }
                        DaemonCommand::Shutdown => {
                            info!("Received Shutdown command");
                            let _ = gui_control_tx.send(GuiControl::Exit);
                            break;
                        }
                        DaemonCommand::Restart => {
                            info!("Received Restart command");
                            restart_requested = true;
                            let _ = gui_control_tx.send(GuiControl::Exit);
                            break;
                        }
                        DaemonCommand::SnapshotCorrection => corrections.snapshot(),
                        _ => {
                            warn!("Ignoring unexpected command in Idle state");
                        }
                    },
                    Ok(None) => {
                        error!("Control command channel closed");
                        break;
                    }
                    Err(_) => {}
                }
            }

            DaemonState::Recording => {
                if let Some(ref task) = audio_task {
                    if task.is_finished() {
                        error!("Audio task died unexpectedly - recovering to Idle");
                        health_state.audio_healthy.store(false, Ordering::Relaxed);
                        *health_state.last_error.write().await =
                            Some("Audio task crashed during recording".to_string());

                        audio_task = None;
                        if let Some(task) = preview_task.take() {
                            let _ = cancel_tx.send(true);
                            let _ = task.await;
                        }
                        let _ = device_manager.stop();
                        let _ = gui_control_tx.send(GuiControl::SetHidden);
                        session = None;
                        _idle_inhibit = None;
                        daemon_state = DaemonState::Idle;
                        let _ = state_tx.send(daemon_state);
                        info!("Recovered to Idle state after audio task crash");
                        continue;
                    }
                }

                match tokio::time::timeout(Duration::from_millis(100), command_rx.recv()).await {
                    Ok(Some(cmd)) => match cmd {
                        DaemonCommand::Confirm => {
                            info!("Received Confirm command");
                            daemon_state = DaemonState::Processing;
                            let _ = state_tx.send(daemon_state);
                        }
                        DaemonCommand::StopRecording => {
                            info!("Received StopRecording (cancel)");

                            let _ = device_manager.stop();

                            let _ = device_manager.flush();

                            let _ = cancel_tx.send(true);

                            if let Some(task) = audio_task.take() {
                                let _ = task.await;
                            }
                            if let Some(task) = preview_task.take() {
                                let _ = task.await;
                            }

                            let _ = gui_control_tx.send(GuiControl::SetHidden);

                            session = None;
                            _idle_inhibit = None;
                            daemon_state = DaemonState::Idle;
                            let _ = state_tx.send(daemon_state);
                            info!("Returned to Idle state");
                        }
                        DaemonCommand::Shutdown => {
                            info!("Shutdown during recording");

                            let _ = device_manager.stop();
                            let _ = device_manager.flush();
                            let _ = cancel_tx.send(true);

                            if let Some(task) = audio_task.take() {
                                let _ = task.await;
                            }
                            if let Some(task) = preview_task.take() {
                                let _ = task.await;
                            }

                            let _ = gui_control_tx.send(GuiControl::Exit);
                            break;
                        }
                        DaemonCommand::Restart => {
                            info!("Restart during recording");
                            restart_requested = true;

                            let _ = device_manager.stop();
                            let _ = device_manager.flush();
                            let _ = cancel_tx.send(true);

                            if let Some(task) = audio_task.take() {
                                let _ = task.await;
                            }
                            if let Some(task) = preview_task.take() {
                                let _ = task.await;
                            }

                            let _ = gui_control_tx.send(GuiControl::Exit);
                            break;
                        }
                        DaemonCommand::SwitchDevice(name) => {
                            warn!("Device switch to {:?} requested during recording, will apply on next session",
                                  name.as_deref().unwrap_or("Default"));
                            device_manager.set_device(name);
                        }
                        _ => {
                            warn!("Ignoring unexpected command in Recording state");
                        }
                    },
                    Ok(None) => {
                        error!("Control command channel closed");
                        break;
                    }
                    Err(_) => {}
                }
            }

            DaemonState::Processing => {
                info!("Entering Processing state");

                if media_was_playing {
                    media_was_playing = false;
                    let delay = config.daemon.media_resume_delay_ms;
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                        platform::resume_media();
                    });
                }

                gui_control_tx
                    .send(GuiControl::SetTranscribing)
                    .map_err(|e| anyhow::anyhow!("Failed to send SetTranscribing: {}", e))?;

                let mut processing_guard = ProcessingGuard::new(gui_control_tx.clone());

                let _ = device_manager.stop();

                let _ = device_manager.flush();

                let _ = cancel_tx.send(true);

                if let Some(task) = audio_task.take() {
                    let _ = task.await;
                }
                if let Some(task) = preview_task.take() {
                    let _ = task.await;
                }

                let session_engine = session
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("No active session in Processing state"))?
                    .engine
                    .clone();

                let audio_buffer_len = session_engine.get_audio_buffer().len();
                info!("Audio buffer contains {} samples", audio_buffer_len);

                if audio_buffer_len > 0 {
                    let mut final_rx = session_engine.subscribe();
                    session_engine.finish();
                    let preview_text = recv_final_transcript(&mut final_rx).await;
                    info!("Transcription: '{}'", preview_text);

                    let final_stages = config.session_stages(&model_spec, session_engine.as_ref());
                    let final_ctx = StageContext {
                        user_dict: Some(Arc::clone(&user_dict)),
                        word_sub: word_sub.clone(),
                        llm: Some(config.llm_correction.clone()),
                    };
                    let pipeline =
                        Pipeline::from_stages(&final_stages, &final_ctx, PipelinePass::Final);
                    let processed_result = pipeline.process(&preview_text)?;

                    if !pipeline.is_empty() && preview_text != processed_result {
                        info!("[Final] Processed: '{}'", processed_result);
                    }

                    if debug_audio::is_debug_audio_enabled() {
                        let audio_buffer = session_engine.get_audio_buffer();
                        let metadata = debug_audio::AudioMetadata {
                            timestamp: chrono::Utc::now(),
                            duration_ms: (audio_buffer.len() as u64 * 1000) / sample_rate as u64,
                            sample_rate,
                            sample_count: audio_buffer.len(),
                            devices: vec![config.daemon.audio_device.clone()],
                            active_device: Some(config.daemon.audio_device.clone()),
                            preview_text: preview_text.clone(),
                            final_text: processed_result.clone(),
                            preview_engine: model_spec.to_string(),
                            accurate_engine: model_spec.to_string(),
                            same_model_used: true,
                        };
                        if let Err(e) =
                            debug_audio::save_debug_audio(&audio_buffer, sample_rate, metadata)
                        {
                            warn!("Failed to save debug audio: {}", e);
                        }
                    }

                    let profile = match &focus_target {
                        Some(target) => app_profile::AppProfile::from_window_class(target.class()),
                        None => app_profile::AppProfile::for_category(
                            window_detect::AppCategory::General,
                        ),
                    };

                    let sanitizer =
                        SanitizationProcessor::new(profile.sanitization.clone(), profile.category);
                    let sanitized_result = sanitizer.process(&processed_result)?;

                    let copied =
                        platform::copy_to_clipboard(&sanitized_result).unwrap_or_else(|e| {
                            warn!("{:#}", e);
                            false
                        });
                    if copied {
                        debug!("Copied to clipboard ({} chars)", sanitized_result.len());
                    }

                    if let Some(ref target) = focus_target {
                        target.refocus().await.ok();
                    }

                    let expected_typing_secs =
                        (sanitized_result.len() as u64 * profile.word_delay_ms) / 1000;
                    if expected_typing_secs > 15 {
                        let fallback = if copied {
                            "; text is already in clipboard if interrupted"
                        } else {
                            ""
                        };
                        warn!(
                            "Typing will take ~{}s ({} chars at {}ms/char){}",
                            expected_typing_secs,
                            sanitized_result.len(),
                            profile.word_delay_ms,
                            fallback
                        );
                    }
                    info!(
                        "Typing final text ({:?} mode, delay={}ms)...",
                        profile.category, profile.word_delay_ms
                    );
                    let typing_tx = gui_control_tx.clone();
                    let mut last_sent = 0usize;
                    platform::type_text_with_progress(
                        &sanitized_result,
                        profile.word_delay_ms,
                        move |done, total| {
                            let step = (total / 50).max(1);
                            if done == 0 || done == total || done - last_sent >= step {
                                last_sent = done;
                                let _ = typing_tx.send(GuiControl::SetTyping { done, total });
                            }
                        },
                    )
                    .await?;
                    info!("Typed!");

                    corrections.record_injection(
                        &sanitized_result,
                        focus_target.as_ref().map(|t| t.class()).unwrap_or_default(),
                    );

                    gui_control_tx
                        .send(GuiControl::SetClosing)
                        .map_err(|e| anyhow::anyhow!("Failed to send SetClosing: {}", e))?;

                    tokio::time::sleep(tokio::time::Duration::from_millis(350)).await;
                } else {
                    info!("No text to type");
                    gui_control_tx
                        .send(GuiControl::SetClosing)
                        .map_err(|e| anyhow::anyhow!("Failed to send SetClosing: {}", e))?;
                    tokio::time::sleep(tokio::time::Duration::from_millis(350)).await;
                }

                gui_control_tx
                    .send(GuiControl::SetHidden)
                    .map_err(|e| anyhow::anyhow!("Failed to send SetHidden: {}", e))?;
                processing_guard.disarm();

                let _ = device_manager.stop();

                session = None;
                _idle_inhibit = None;
                engine_stopped_at = Some(Instant::now());
                daemon_state = DaemonState::Idle;
                let _ = state_tx.send(daemon_state);
                info!("Processing complete - returned to Idle state");
            }
        }
    }

    info!("Daemon shutting down");
    if restart_requested {
        info!("Exiting with code 64 so the service manager restarts the daemon");
        std::process::exit(64);
    }
    Ok(())
}

#[derive(Debug)]
pub struct FileTranscription {
    pub model: String,
    pub stages: String,
    pub raw: String,
    pub processed: String,
    pub partials: usize,
    pub first_partial_ms: Option<u128>,
    pub final_ms: u128,
    pub audio_secs: f32,
}

pub async fn transcribe_file(
    wav: &std::path::Path,
    model_override: Option<&str>,
    stages_override: Option<&str>,
    paced: bool,
) -> Result<FileTranscription> {
    let config = load_config()?;
    let mut reader = hound::WavReader::open(wav)
        .map_err(|e| anyhow::anyhow!("opening {}: {e}", wav.display()))?;
    let spec_wav = reader.spec();
    if spec_wav.sample_rate != 16_000 || spec_wav.channels != 1 || spec_wav.bits_per_sample != 16 {
        anyhow::bail!(
            "need 16 kHz mono 16-bit WAV, got {} Hz, {} ch, {} bit (convert with: ffmpeg -i in.wav -ac 1 -ar 16000 -sample_fmt s16 out.wav)",
            spec_wav.sample_rate,
            spec_wav.channels,
            spec_wav.bits_per_sample
        );
    }
    let samples: Vec<i16> = reader.samples::<i16>().collect::<std::result::Result<_, _>>()?;

    let spec = ModelSpec::parse(model_override.unwrap_or(&config.daemon.model))?;
    let user_dict = Arc::new(UserDictionary::new().unwrap_or_else(|_| UserDictionary::empty()));
    let word_sub = if config.daemon.enable_word_substitution {
        WordSubstitutionProcessor::new(Some(Arc::clone(&user_dict))).ok()
    } else {
        None
    };
    let engine = spec.create_streaming_engine(16_000, &config.engine_options(&user_dict))?;
    let stages = match stages_override {
        Some(list) => resolve_stages(&engine.default_stages(), Some(list), config.stage_switches()),
        None => config.session_stages(&spec, engine.as_ref()),
    };

    engine.reset();
    let mut rx = engine.subscribe();
    let started = Instant::now();
    let mut partials = 0usize;
    let mut first_partial_ms = None;
    for chunk in samples.chunks(1_600) {
        engine.process_audio(chunk)?;
        if paced {
            let tick = tokio::time::sleep(Duration::from_millis(100));
            tokio::pin!(tick);
            loop {
                tokio::select! {
                    _ = &mut tick => break,
                    ev = rx.recv() => if let Some(TranscriptEvent::Partial(_)) = ev {
                        partials += 1;
                        first_partial_ms.get_or_insert(started.elapsed().as_millis());
                    },
                }
            }
        }
    }

    let stopped = Instant::now();
    engine.finish();
    let raw = loop {
        match tokio::time::timeout(Duration::from_secs(120), rx.recv()).await {
            Ok(Some(TranscriptEvent::Partial(_))) => {
                partials += 1;
                first_partial_ms.get_or_insert(started.elapsed().as_millis());
            }
            Ok(Some(TranscriptEvent::Final(t))) => break t,
            Ok(Some(TranscriptEvent::Error(e))) => anyhow::bail!("engine error: {e}"),
            Ok(None) => anyhow::bail!("engine event stream closed before the final transcript"),
            Err(_) => anyhow::bail!("timed out waiting for the final transcript"),
        }
    };
    let final_ms = stopped.elapsed().as_millis();

    let ctx = StageContext {
        user_dict: Some(Arc::clone(&user_dict)),
        word_sub,
        llm: Some(config.llm_correction.clone()),
    };
    let processed = Pipeline::from_stages(&stages, &ctx, PipelinePass::Final).process(&raw)?;

    Ok(FileTranscription {
        model: spec.to_string(),
        stages: describe_stages(&stages),
        raw,
        processed,
        partials,
        first_partial_ms,
        final_ms,
        audio_secs: samples.len() as f32 / 16_000.0,
    })
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn legacy_config_parses_and_keeps_local_chain() {
        let cfg: Config = toml::from_str(
            r#"
            [daemon]
            audio_device = "all"
            sample_rate = "16000"
            model = "parakeet:default"
            enable_grammar = true
            "#,
        )
        .unwrap();
        assert!(cfg.pipeline.parakeet.is_none());
        let switches = cfg.stage_switches();
        let stages = resolve_stages(
            post_processing::LOCAL_MODEL_STAGES,
            cfg.pipeline.for_provider(Provider::Parakeet),
            switches,
        );
        assert_eq!(stages, post_processing::LOCAL_MODEL_STAGES);
        assert!(!cfg.llm_correction.is_configured());
        assert_eq!(cfg.openai.delay, "low");
    }

    #[test]
    fn tui_written_config_parses() {
        let cfg: Config = toml::from_str(
            r#"
            [daemon]
            audio_device = "default"
            sample_rate = "16000"
            model = "openai:gpt-live-transcribe"
            enable_grammar = false

            [pipeline]
            parakeet = "default"
            openai = "default"

            [openai]
            prompt = ""
            keywords_from_dictionary = true
            extra_keywords = "WorkOS, JWKS"
            languages = "en"
            delay = "low"
            realtime_url = "wss://api.openai.com/v1/realtime?intent=transcription"

            [llm_correction]
            model = ""
            region = "us-west-2"
            aws_profile = ""
            timeout_ms = 3000
            "#,
        )
        .unwrap();
        let stages =
            resolve_stages(&[], cfg.pipeline.for_provider(Provider::OpenAi), cfg.stage_switches());
        assert!(stages.is_empty());
        let local = resolve_stages(
            post_processing::LOCAL_MODEL_STAGES,
            cfg.pipeline.for_provider(Provider::Parakeet),
            cfg.stage_switches(),
        );
        assert!(!local.contains(&Stage::Grammar));
        let opts = cfg.openai.to_options(&[]);
        assert_eq!(opts.keywords, vec!["WorkOS", "JWKS"]);
        assert_eq!(opts.languages, vec!["en"]);
    }

    #[test]
    fn schema_covers_all_config_sections() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../config-schema.json")).unwrap();
        let ids: Vec<&str> = schema["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        for section in ["daemon", "pipeline", "openai", "llm_correction"] {
            assert!(ids.contains(&section), "schema is missing section [{section}]");
        }
    }
}
