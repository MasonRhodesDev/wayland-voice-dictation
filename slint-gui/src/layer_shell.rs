use dictation_types::{GuiControl, GuiState, GuiStatus};
use hypr_slint_runtime::WakeHandle;
use layer_shika::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info};

use crate::{
    apply_state, check_ui_contract, compile_ui, get_ui_config_dir, monitor, spawn_channel_listener,
    spawn_ui_clock, spawn_ui_file_watcher, GuiResult, SharedState, Spinner, UiClock,
    EXIT_CODE_RELOAD, REQUIRED_UI_PROPERTIES,
};

const EXIT_CODE_SURFACES_LOST: i32 = 1;

const SURFACE_LOSS_GRACE: Duration = Duration::from_secs(10);

const COMPOSITOR_SETTLE: Duration = Duration::from_secs(3);

impl UiClock {
    fn note_surface_count(&self, count: usize) {
        let mut empty_since = self.empty_since.lock().expect("surface deadline poisoned");
        match (count, empty_since.is_some()) {
            (0, false) => *empty_since = Some(Instant::now()),
            (1.., true) => *empty_since = None,
            _ => {}
        }
        drop(empty_since);
        self.changed.notify_one();
    }

    pub(crate) fn next_maintenance(&self, now: Instant) -> Option<Duration> {
        let topology = monitor::pending_compositor_change()
            .map(|changed| changed + COMPOSITOR_SETTLE)
            .map(|deadline| deadline.saturating_duration_since(now));
        let surfaces = *self.empty_since.lock().expect("surface deadline poisoned");
        let surfaces = surfaces
            .map(|empty| empty + SURFACE_LOSS_GRACE)
            .map(|deadline| deadline.saturating_duration_since(now));
        [topology, surfaces].into_iter().flatten().min()
    }
}

fn resolve_ui_path(name: &str) -> String {
    if let Some(config_dir) = get_ui_config_dir() {
        let config_path = config_dir.join(format!("{}.slint", name));
        if config_path.exists() {
            return config_path.to_string_lossy().to_string();
        }
    }

    format!("ui/{}.slint", name)
}

pub fn run_integrated(
    gui_control_tx: broadcast::Sender<GuiControl>,
    spectrum_tx: broadcast::Sender<Vec<f32>>,
    gui_status_tx: mpsc::Sender<GuiStatus>,
    runtime_handle: tokio::runtime::Handle,
) -> GuiResult<()> {
    info!("Starting slint-gui (integrated mode)");

    let shared_state = Arc::new(RwLock::new(SharedState::default()));

    let (wake_sender, wake_receiver) = layer_shika::calloop::channel::channel();
    let wake = WakeHandle::new(move || {
        let _ = wake_sender.send(());
    });
    let clock = UiClock::new(wake);
    let mut wake_receiver = Some(wake_receiver);

    let reload_flag = Arc::new(AtomicBool::new(false));

    let gui_control_rx = gui_control_tx.subscribe();
    let spectrum_rx = spectrum_tx.subscribe();

    spawn_channel_listener(
        gui_control_rx,
        spectrum_rx,
        shared_state.clone(),
        gui_status_tx.clone(),
        runtime_handle.clone(),
        clock.clone(),
    );

    let monitor_clock = clock.clone();
    monitor::spawn_active_monitor_listener(Arc::new(move || monitor_clock.signal()));

    spawn_ui_file_watcher(reload_flag.clone(), clock.clone());

    spawn_ui_clock(shared_state.clone(), clock.clone());

    info!("Creating Wayland layer shell (this may take a few seconds)...");
    let mut backoff = Duration::from_secs(1);
    loop {
        match run_shell(
            shared_state.clone(),
            reload_flag.clone(),
            gui_status_tx.clone(),
            clock.clone(),
            &mut wake_receiver,
        ) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let msg = e.to_string();
                if msg.to_lowercase().contains("platform") {
                    error!(
                        "Shell creation failed after Slint platform init ({msg}); \
                         exiting for systemd restart"
                    );
                    std::process::exit(EXIT_CODE_SURFACES_LOST);
                }

                error!("Failed to create shell: {msg}; retrying in {}s", backoff.as_secs());
                let _ = gui_status_tx.blocking_send(GuiStatus::Error(format!(
                    "Wayland layer shell initialization failed: {msg}. Retrying in {}s.",
                    backoff.as_secs()
                )));
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

fn run_shell(
    shared_state: Arc<RwLock<SharedState>>,
    reload_flag: Arc<AtomicBool>,
    gui_status_tx: mpsc::Sender<GuiStatus>,
    clock: Arc<UiClock>,
    wake_receiver: &mut Option<layer_shika::calloop::channel::Channel<()>>,
) -> GuiResult<()> {
    let ui_file = resolve_ui_path("dictation");
    info!("Loading UI from: {}", ui_file);

    match compile_ui(&ui_file, None, None)
        .and_then(|definition| check_ui_contract(&ui_file, &definition))
    {
        Err(e) => error!("UI contract validation failed: {e}"),
        Ok(()) => {
            info!("UI contract OK ({} required properties present)", REQUIRED_UI_PROPERTIES.len())
        }
    }

    info!("Creating Shell from UI file...");
    let mut runtime = Shell::from_file(&ui_file)
        .surface("Dictation")
        .width(380)
        .height(90)
        .anchor(AnchorEdges::empty().with_bottom())
        .margin((0, 0, 50, 0))
        .layer(Layer::Overlay)
        .keyboard_interactivity(KeyboardInteractivity::None)
        .output_policy(OutputPolicy::AllOutputs)
        .build()
        .map_err(|e| format!("Failed to create shell: {}", e))?;

    info!("Shell created successfully");

    if let Err(e) = gui_status_tx.blocking_send(GuiStatus::Ready) {
        error!("Failed to send ready status: {}", e);
    } else {
        info!("Sent Ready status to daemon - GUI is operational");
    }

    info!("Getting event loop handle...");
    let event_loop = runtime.event_loop_handle();
    info!("Got event loop handle");

    let mut gui_initialized = false;
    let mut last_logged_state: Option<GuiState> = None;
    let mut spinner = Spinner::new(Instant::now());

    let wake_for_callback = clock.wake.clone();
    let clock_for_callback = clock.clone();
    let wake_receiver = wake_receiver.take().ok_or("UI wake source was already consumed")?;
    event_loop
        .insert_source(wake_receiver, move |event, (), app_state| {
            if !matches!(event, layer_shika::calloop::channel::Event::Msg(())) {
                return;
            }
            wake_for_callback.acknowledge();

            if reload_flag.load(Ordering::SeqCst) {
                info!("UI file changed, reloading shell...");
                reload_flag.store(false, Ordering::SeqCst);
                std::process::exit(EXIT_CODE_RELOAD);
            }

            if let Some(changed_at) = monitor::pending_compositor_change() {
                if changed_at.elapsed() >= COMPOSITOR_SETTLE {
                    monitor::clear_compositor_change();
                    if let Some(expected) = monitor::get_monitor_names_sync() {
                        let present: std::collections::HashSet<String> = app_state
                            .surfaces_with_keys()
                            .filter_map(|(key, _)| {
                                app_state
                                    .get_output_info(key.output_handle)
                                    .and_then(|info| info.name().map(|n| n.to_string()))
                            })
                            .collect();
                        let missing: Vec<&String> =
                            expected.iter().filter(|n| !present.contains(*n)).collect();
                        if missing.is_empty() {
                            info!(
                                "Compositor change reconciled in place ({} surfaces, monitors {:?})",
                                present.len(),
                                expected
                            );
                        } else {
                            error!(
                                "Monitors {:?} have no overlay surface after compositor change, \
                                 exiting for systemd restart",
                                missing
                            );
                            std::process::exit(EXIT_CODE_SURFACES_LOST);
                        }
                    } else {
                        debug!("Hyprland IPC unavailable, skipping surface verification");
                    }
                }
            }

            let surface_count = app_state.surfaces_with_keys().count();
            if surface_count > 0 {
                gui_initialized = true;
                clock_for_callback.note_surface_count(surface_count);
            } else if gui_initialized {
                clock_for_callback.note_surface_count(0);
                let expired = clock_for_callback
                    .empty_since
                    .lock()
                    .expect("surface deadline poisoned")
                    .is_some_and(|since| since.elapsed() >= SURFACE_LOSS_GRACE);
                if expired {
                    error!(
                        "All layer surfaces lost for ~{}s after init, exiting for systemd restart",
                        SURFACE_LOSS_GRACE.as_secs()
                    );
                    std::process::exit(EXIT_CODE_SURFACES_LOST);
                }
            }

            let active_monitor = monitor::get_active_monitor();

            if let Ok(state) = shared_state.read() {
                let spinner_angle = spinner.advance(state.gui_state, Instant::now());
                if last_logged_state != Some(state.gui_state) {
                    if state.gui_state != GuiState::Hidden {
                        info!("GUI state -> {:?} (active_monitor={:?})", state.gui_state, active_monitor);
                    } else {
                        debug!("GUI state -> Hidden");
                    }
                    last_logged_state = Some(state.gui_state);
                }

                let use_all_monitors = active_monitor.is_none()
                    || active_monitor.as_ref().is_some_and(|s| s.is_empty());
                if use_all_monitors && state.gui_state != GuiState::Hidden {
                    debug!("Monitor detection unavailable, showing GUI on all monitors");
                }

                for (key, surface_state) in app_state.surfaces_with_keys() {
                    let component = surface_state.component_instance();

                    let output_name = app_state.get_output_info(key.output_handle)
                        .and_then(|info| info.name().map(|n| n.to_string()));

                    let is_active = if use_all_monitors {
                        state.gui_state != GuiState::Hidden
                    } else if let Some(ref active_name) = active_monitor {
                        output_name.as_ref()
                            .map(|name| name == active_name)
                            .unwrap_or(false)
                    } else {
                        app_state.get_output_info(key.output_handle)
                            .map(|info| info.is_primary())
                            .unwrap_or(false)
                    };

                    apply_state(component, &state, is_active, spinner_angle);
                }
            }

        })
        .map_err(|e| format!("Failed to add UI wake source: {}", e))?;
    clock.signal();

    info!("Event-driven UI wake source ready; starting shell event loop");
    info!("Starting shell event loop");
    runtime.run().map_err(|e| format!("Shell run error: {}", e))?;

    error!("Shell event loop exited unexpectedly, exiting for systemd restart");
    std::process::exit(EXIT_CODE_SURFACES_LOST);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_deadlines_do_not_become_hidden_polling() {
        let clock = UiClock::new(WakeHandle::new(|| {}));
        assert_eq!(clock.next_maintenance(Instant::now()), None);

        clock.note_surface_count(0);
        let remaining = clock.next_maintenance(Instant::now()).expect("surface deadline");
        assert!(remaining <= SURFACE_LOSS_GRACE);
        assert!(remaining > Duration::from_secs(9));

        clock.note_surface_count(1);
        assert_eq!(clock.next_maintenance(Instant::now()), None);
    }
}
