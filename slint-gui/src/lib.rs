use dictation_types::{GuiControl, GuiState, GuiStatus};
use hypr_slint_runtime::{IdleScheduler, WaitDecision, WakeHandle};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use slint_interpreter::{ComponentDefinition, ComponentInstance, Value};
use std::env;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};

#[cfg(target_os = "linux")]
mod layer_shell;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod monitor;
#[cfg(any(target_os = "macos", test))]
mod placement;

#[cfg(target_os = "linux")]
pub use layer_shell::run_integrated;
#[cfg(target_os = "macos")]
pub use macos::{attach_overlay, init, quit, run_on_ui_thread, run_until_quit};
#[cfg(target_os = "linux")]
pub use monitor::get_active_monitor_sync;

pub struct SharedState {
    pub gui_state: GuiState,
    pub transcription: String,
    pub spectrum_values: Vec<f32>,
    pub closing_progress: f32,
    pub fade: f32,
    pub pre_listening: bool,
    pub typing_progress: f32,
}

struct UiClock {
    wake: WakeHandle,
    generation: Mutex<u64>,
    changed: Condvar,
    #[cfg(target_os = "linux")]
    empty_since: Mutex<Option<Instant>>,
}

impl UiClock {
    fn new(wake: WakeHandle) -> Arc<Self> {
        Arc::new(Self {
            wake,
            generation: Mutex::new(0),
            changed: Condvar::new(),
            #[cfg(target_os = "linux")]
            empty_since: Mutex::new(None),
        })
    }

    fn signal(&self) {
        self.wake.wake();
        let mut generation = self.generation.lock().expect("UI clock poisoned");
        *generation = generation.wrapping_add(1);
        self.changed.notify_one();
    }
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            gui_state: GuiState::Hidden,
            transcription: String::new(),
            spectrum_values: vec![0.0; 8],
            closing_progress: 0.0,
            fade: 1.0,
            pre_listening: false,
            typing_progress: 0.0,
        }
    }
}

fn get_ui_config_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| {
        let mut path = PathBuf::from(home);
        path.push(".config/voice-dictation/ui");
        path
    })
}

fn spawn_ui_file_watcher(reload_flag: Arc<AtomicBool>, clock: Arc<UiClock>) {
    let Some(ui_dir) = get_ui_config_dir() else {
        info!("No UI config directory found, hot-reload disabled");
        return;
    };

    if !ui_dir.exists() {
        info!("UI config directory doesn't exist: {:?}, hot-reload disabled", ui_dir);
        return;
    }

    std::thread::spawn(move || {
        let reload_flag_clone = reload_flag.clone();
        let clock_clone = clock.clone();
        let mut watcher: RecommendedWatcher = match notify::recommended_watcher(
            move |res: std::result::Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    if event.kind.is_modify() || event.kind.is_create() {
                        let is_slint = event
                            .paths
                            .iter()
                            .any(|p| p.extension().is_some_and(|ext| ext == "slint"));
                        if is_slint {
                            info!("UI file changed, triggering reload...");
                            reload_flag_clone.store(true, Ordering::SeqCst);
                            clock_clone.signal();
                        }
                    }
                }
            },
        ) {
            Ok(w) => w,
            Err(e) => {
                error!("Failed to create file watcher: {}", e);
                return;
            }
        };

        if let Err(e) = watcher.watch(&ui_dir, RecursiveMode::NonRecursive) {
            error!("Failed to watch UI directory {:?}: {}", ui_dir, e);
            return;
        }

        info!("Watching UI directory for changes: {:?}", ui_dir);

        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    });
}

pub type GuiResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn spawn_channel_listener(
    mut gui_control_rx: broadcast::Receiver<GuiControl>,
    mut spectrum_rx: broadcast::Receiver<Vec<f32>>,
    shared_state: Arc<RwLock<SharedState>>,
    gui_status_tx: mpsc::Sender<GuiStatus>,
    runtime_handle: tokio::runtime::Handle,
    clock: Arc<UiClock>,
) {
    let state_clone = shared_state.clone();
    let status_tx = gui_status_tx.clone();
    let control_clock = clock.clone();
    runtime_handle.spawn(async move {
        loop {
            match gui_control_rx.recv().await {
                Ok(control) => {
                    if let Ok(mut state) = state_clone.write() {
                        let old_state = state.gui_state;
                        match control {
                            GuiControl::Initialize => {
                                state.gui_state = GuiState::Hidden;
                            }
                            GuiControl::SetHidden => {
                                state.gui_state = GuiState::Hidden;
                            }
                            GuiControl::SetListening => {
                                state.gui_state = GuiState::Listening;
                                state.fade = 1.0;
                                state.pre_listening = false;
                            }
                            GuiControl::UpdateTranscription { text, .. } => {
                                state.transcription = text;
                            }
                            GuiControl::UpdateSpectrum(values) => {
                                state.spectrum_values = values;
                            }
                            GuiControl::UpdateVadState { .. } => {}
                            GuiControl::SetTranscribing => {
                                state.gui_state = GuiState::Transcribing;
                                state.fade = 1.0;
                                state.typing_progress = 0.0;
                            }
                            GuiControl::SetTyping { done, total } => {
                                state.gui_state = GuiState::Typing;
                                state.fade = 1.0;
                                state.typing_progress = if total > 0 {
                                    (done as f32 / total as f32).clamp(0.0, 1.0)
                                } else {
                                    0.0
                                };
                            }
                            GuiControl::SetClosing => {
                                state.gui_state = GuiState::Closing;
                                state.closing_progress = 0.0;
                            }
                            GuiControl::Exit => {
                                info!("Received Exit command");
                                std::process::exit(0);
                            }
                        }

                        let new_state = state.gui_state;
                        if old_state != new_state {
                            debug!("State transition: {:?} -> {:?}", old_state, new_state);
                            let _ = status_tx.try_send(GuiStatus::TransitionComplete {
                                from: old_state,
                                to: new_state,
                            });
                        }
                    }
                    control_clock.signal();
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("Control channel lagged by {} messages", n);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    info!("Control channel closed");
                    break;
                }
            }
        }
    });

    let state_clone = shared_state.clone();
    let spectrum_clock = clock;
    runtime_handle.spawn(async move {
        loop {
            match spectrum_rx.recv().await {
                Ok(raw_samples) => {
                    let bands = compute_spectrum_bands(&raw_samples);
                    if let Ok(mut state) = state_clone.write() {
                        state.spectrum_values = bands;
                    }
                    spectrum_clock.signal();
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

fn state_has_visible_animation(state: GuiState) -> bool {
    matches!(state, GuiState::Transcribing | GuiState::Typing | GuiState::Closing)
}

fn spawn_ui_clock(shared_state: Arc<RwLock<SharedState>>, clock: Arc<UiClock>) {
    std::thread::Builder::new()
        .name("voice-ui-clock".into())
        .spawn(move || {
            let scheduler = IdleScheduler::default();
            let mut observed_generation = 0;
            loop {
                let animated = shared_state
                    .read()
                    .map(|state| state_has_visible_animation(state.gui_state))
                    .unwrap_or(false);
                let decision = scheduler.decide(animated, clock.next_maintenance(Instant::now()));

                let mut generation = clock.generation.lock().expect("UI clock poisoned");
                if *generation != observed_generation {
                    observed_generation = *generation;
                    continue;
                }

                match decision {
                    WaitDecision::Indefinite => {
                        generation = clock.changed.wait(generation).expect("UI clock poisoned");
                        observed_generation = *generation;
                    }
                    WaitDecision::Frame(delay) | WaitDecision::Timer(delay) => {
                        let (next, timeout) = clock
                            .changed
                            .wait_timeout(generation, delay)
                            .expect("UI clock poisoned");
                        generation = next;
                        observed_generation = *generation;
                        if timeout.timed_out() {
                            drop(generation);
                            clock.signal();
                        }
                    }
                }
            }
        })
        .expect("failed to start UI clock");
}

fn compute_spectrum_bands(samples: &[f32]) -> Vec<f32> {
    let len = samples.len();
    if len == 0 {
        return vec![0.0; 8];
    }

    let chunk_size = len / 8;
    if chunk_size == 0 {
        return vec![0.0; 8];
    }

    let mut bands = Vec::with_capacity(8);

    for i in 0..8 {
        let start = i * chunk_size;
        let end = if i == 7 { len } else { (i + 1) * chunk_size };
        let chunk = &samples[start..end];

        let sum: f32 = chunk.iter().map(|&x| x * x).sum();
        let rms = (sum / chunk.len() as f32).sqrt();

        let normalized = (rms * 15.0).min(1.0);
        bands.push(normalized);
    }

    bands
}

fn state_to_mode(state: GuiState) -> i32 {
    match state {
        GuiState::Hidden => 0,
        GuiState::PreListening => 1,
        GuiState::Listening => 1,
        GuiState::Transcribing => 2,
        GuiState::Typing => 2,
        GuiState::Closing => 3,
    }
}

const EXIT_CODE_RELOAD: i32 = 64;

const REQUIRED_UI_PROPERTIES: &[&str] = &[
    "mode",
    "spinner-angle",
    "progress",
    "fade",
    "spectrum",
    "text",
    "pre-listening",
    "closing-progress",
];

fn compile_ui(
    ui_file: &str,
    source: Option<String>,
    component: Option<&str>,
) -> std::result::Result<ComponentDefinition, String> {
    let compiler = slint_interpreter::Compiler::default();
    let result = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(|e| format!("could not start UI validation runtime: {e}"))?
        .block_on(async {
            match source {
                Some(source) => compiler.build_from_source(source, PathBuf::from(ui_file)).await,
                None => compiler.build_from_path(ui_file).await,
            }
        });

    let errors: Vec<String> = result
        .diagnostics()
        .filter(|d| d.level() == slint_interpreter::DiagnosticLevel::Error)
        .map(|d| d.to_string())
        .collect();
    if !errors.is_empty() {
        return Err(format!("UI '{ui_file}' failed to compile: {}", errors.join("; ")));
    }

    match component {
        Some(name) => result.component(name),
        None => result.components().next(),
    }
    .ok_or_else(|| format!("UI '{ui_file}' defines no component"))
}

fn check_ui_contract(
    ui_file: &str,
    definition: &ComponentDefinition,
) -> std::result::Result<(), String> {
    let present: std::collections::HashSet<String> =
        definition.properties().map(|(name, _)| name.replace('-', "_")).collect();
    let missing: Vec<&str> = REQUIRED_UI_PROPERTIES
        .iter()
        .copied()
        .filter(|p| !present.contains(&p.replace('-', "_")))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "UI '{ui_file}' is missing required input properties {missing:?}; it has drifted \
             from the Rust property contract. Add them (see slint-gui/ui/dictation.slint)."
        ));
    }
    Ok(())
}

struct Spinner {
    angle: f32,
    last_frame: Instant,
}

impl Spinner {
    fn new(now: Instant) -> Self {
        Self { angle: 0.0, last_frame: now }
    }

    fn advance(&mut self, state: GuiState, now: Instant) -> f32 {
        if state_has_visible_animation(state) {
            self.angle =
                (self.angle + 360.0 * now.duration_since(self.last_frame).as_secs_f32()) % 360.0;
        }
        self.last_frame = now;
        self.angle
    }
}

fn set_ui_property(component: &ComponentInstance, name: &str, value: Value) {
    if let Err(e) = component.set_property(name, value) {
        debug!("Failed to set {}: {}", name, e);
    }
}

fn apply_state(
    component: &ComponentInstance,
    state: &SharedState,
    is_active: bool,
    spinner_angle: f32,
) {
    let mode = if is_active { state_to_mode(state.gui_state) } else { 0 };
    set_ui_property(component, "mode", Value::Number(mode as f64));
    if !is_active {
        return;
    }

    set_ui_property(component, "spinner-angle", Value::Number(spinner_angle as f64));
    set_ui_property(component, "progress", Value::Number(state.typing_progress as f64));

    if state.gui_state == GuiState::Listening || state.gui_state == GuiState::PreListening {
        let spectrum: [Value; 8] = std::array::from_fn(|band| {
            Value::Number(state.spectrum_values.get(band).copied().unwrap_or(0.0) as f64)
        });
        set_ui_property(component, "spectrum", Value::Model(spectrum.into()));
        set_ui_property(component, "text", Value::String(state.transcription.clone().into()));
        set_ui_property(component, "pre-listening", Value::Bool(state.pre_listening));
    }

    set_ui_property(component, "fade", Value::Number(state.fade as f64));

    if state.gui_state == GuiState::Closing {
        set_ui_property(
            component,
            "closing-progress",
            Value::Number(state.closing_progress as f64),
        );
    }
}

#[cfg(any(target_os = "macos", test))]
fn overlay_visible(state: GuiState) -> bool {
    state_to_mode(state) != 0
}

#[cfg(test)]
mod idle_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn hidden_and_event_driven_listening_do_not_run_a_frame_clock() {
        let scheduler = IdleScheduler::default();
        for state in [GuiState::Hidden, GuiState::PreListening, GuiState::Listening] {
            assert!(!state_has_visible_animation(state));
            assert_eq!(scheduler.decide(false, None), WaitDecision::Indefinite);
        }
    }

    #[test]
    fn spinner_states_request_frames_and_hidden_defrosts_on_one_edge() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let wake = WakeHandle::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        });
        assert!(wake.wake());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        assert!(wake.acknowledge());
        assert!(wake.wake());
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let scheduler = IdleScheduler::new(Duration::from_millis(16));
        for state in [GuiState::Transcribing, GuiState::Typing, GuiState::Closing] {
            assert!(state_has_visible_animation(state));
            assert_eq!(
                scheduler.decide(true, None),
                WaitDecision::Frame(Duration::from_millis(16))
            );
        }
    }

    #[test]
    fn the_overlay_is_visible_in_every_state_but_hidden() {
        assert!(!overlay_visible(GuiState::Hidden));
        for state in [
            GuiState::PreListening,
            GuiState::Listening,
            GuiState::Transcribing,
            GuiState::Typing,
            GuiState::Closing,
        ] {
            assert!(overlay_visible(state));
        }
    }

    #[test]
    fn the_spinner_only_turns_while_an_animated_state_is_shown() {
        let start = Instant::now();
        let mut spinner = Spinner::new(start);
        assert_eq!(spinner.advance(GuiState::Listening, start + Duration::from_millis(250)), 0.0);
        assert_eq!(
            spinner.advance(GuiState::Transcribing, start + Duration::from_millis(500)),
            90.0
        );
        assert_eq!(spinner.advance(GuiState::Hidden, start + Duration::from_secs(5)), 90.0);
        assert_eq!(spinner.advance(GuiState::Typing, start + Duration::from_secs(6)), 90.0);
    }
}
