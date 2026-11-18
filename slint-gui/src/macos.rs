use dictation_types::{GuiControl, GuiStatus};
use hypr_slint_runtime::WakeHandle;
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSScreen, NSStatusWindowLevel, NSView, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSSize};
use slint::winit_030::winit::event_loop::EventLoop;
use slint::winit_030::winit::platform::macos::{
    ActivationPolicy, EventLoopBuilderExtMacOS, WindowAttributesExtMacOS,
};
use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::winit_030::winit::window::Window;
use slint::winit_030::{SlintEvent, WinitWindowAccessor};
use slint::ComponentHandle;
use slint_interpreter::{ComponentDefinition, ComponentInstance};
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};

use crate::placement::{bottom_center, Rect, Size};
use crate::{
    apply_state, compile_overlay, get_ui_config_dir, overlay_visible, spawn_channel_listener,
    spawn_ui_clock, spawn_ui_file_watcher, GuiResult, SharedState, Spinner, UiClock, DEFAULT_UI,
    EXIT_CODE_RELOAD,
};

const OVERLAY_SIZE: Size = Size { width: 380.0, height: 90.0 };
const BOTTOM_MARGIN: f64 = 50.0;
const HIDE_DELAY: Duration = Duration::from_millis(100);

thread_local! {
    static OVERLAY: RefCell<Option<Overlay>> = const { RefCell::new(None) };
}

impl UiClock {
    pub(crate) fn next_maintenance(&self, _now: Instant) -> Option<Duration> {
        None
    }
}

struct Overlay {
    instance: ComponentInstance,
    window: Option<Retained<NSWindow>>,
    ordered_in: bool,
    hide_timer: slint::Timer,
    spinner: Spinner,
    shared_state: Arc<RwLock<SharedState>>,
    clock: Arc<UiClock>,
    reload_flag: Arc<AtomicBool>,
}

pub fn init() -> GuiResult<()> {
    if MainThreadMarker::new().is_none() {
        return Err("the UI event loop must be created on the main thread".into());
    }
    let mut event_loop_builder = EventLoop::<SlintEvent>::with_user_event();
    event_loop_builder
        .with_activation_policy(ActivationPolicy::Accessory)
        .with_activate_ignoring_other_apps(false);
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .with_winit_event_loop_builder(event_loop_builder)
        .with_winit_window_attributes_hook(|attributes| attributes.with_has_shadow(false))
        .select()
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn run_until_quit() -> GuiResult<()> {
    slint::run_event_loop_until_quit().map_err(|e| e.to_string())?;
    Ok(())
}

pub fn quit() {
    run_on_ui_thread(|| {
        let _ = slint::quit_event_loop();
    });
}

pub fn run_on_ui_thread(task: impl FnOnce() + Send + 'static) {
    if let Err(e) = slint::invoke_from_event_loop(task) {
        warn!("UI event loop is not reachable: {e}");
    }
}

pub fn attach_overlay(
    gui_control_tx: broadcast::Sender<GuiControl>,
    spectrum_tx: broadcast::Sender<Vec<f32>>,
    gui_status_tx: mpsc::Sender<GuiStatus>,
    runtime_handle: tokio::runtime::Handle,
) {
    let shared_state = Arc::new(RwLock::new(SharedState::default()));
    let clock = UiClock::new(WakeHandle::new(|| run_on_ui_thread(tick)));
    let reload_flag = Arc::new(AtomicBool::new(false));

    {
        let shared_state = shared_state.clone();
        let clock = clock.clone();
        let reload_flag = reload_flag.clone();
        let gui_status_tx = gui_status_tx.clone();
        run_on_ui_thread(move || create_overlay(shared_state, clock, reload_flag, gui_status_tx));
    }

    spawn_channel_listener(
        gui_control_tx.subscribe(),
        spectrum_tx.subscribe(),
        shared_state.clone(),
        gui_status_tx,
        runtime_handle,
        clock.clone(),
    );
    spawn_ui_file_watcher(reload_flag, clock.clone());
    spawn_ui_clock(shared_state, clock);
}

fn load_overlay() -> Result<ComponentDefinition, String> {
    let user_ui =
        get_ui_config_dir().map(|dir| dir.join("dictation.slint")).filter(|path| path.exists());
    if let Some(path) = user_ui {
        let ui_file = path.to_string_lossy();
        info!("Loading UI from: {}", ui_file);
        match compile_overlay(&ui_file, None) {
            Ok(definition) => return Ok(definition),
            Err(e) => error!("{e}; falling back to the built-in UI"),
        }
    }
    compile_overlay("dictation.slint", Some(DEFAULT_UI.to_string()))
}

fn create_overlay(
    shared_state: Arc<RwLock<SharedState>>,
    clock: Arc<UiClock>,
    reload_flag: Arc<AtomicBool>,
    gui_status_tx: mpsc::Sender<GuiStatus>,
) {
    let shown = load_overlay().and_then(|definition| {
        let instance = definition.create().map_err(|e| e.to_string())?;
        instance.window().set_size(slint::LogicalSize::new(
            OVERLAY_SIZE.width as f32,
            OVERLAY_SIZE.height as f32,
        ));
        instance.show().map_err(|e| e.to_string())?;
        Ok(instance)
    });
    let instance = match shown {
        Ok(instance) => instance,
        Err(e) => {
            return give_up(&gui_status_tx, format!("Overlay window could not be created: {e}"))
        }
    };

    let pending = instance.clone_strong();
    OVERLAY.with_borrow_mut(|overlay| {
        *overlay = Some(Overlay {
            instance,
            window: None,
            ordered_in: true,
            hide_timer: slint::Timer::default(),
            spinner: Spinner::new(Instant::now()),
            shared_state,
            clock,
            reload_flag,
        });
    });

    let scheduling_status_tx = gui_status_tx.clone();
    let attached = slint::spawn_local(async move {
        let window = match pending.window().winit_window().await {
            Ok(winit_window) => ns_window(&winit_window),
            Err(e) => {
                return give_up(&gui_status_tx, format!("Overlay window was not created: {e}"))
            }
        };
        let Some(window) = window else {
            return give_up(&gui_status_tx, "Overlay has no AppKit window".to_string());
        };
        configure(&window);
        OVERLAY.with_borrow_mut(|overlay| {
            if let Some(overlay) = overlay {
                overlay.window = Some(window);
                overlay.ordered_in = false;
            }
        });
        if let Err(e) = gui_status_tx.try_send(GuiStatus::Ready) {
            error!("Failed to send ready status: {}", e);
        }
        tick();
    });
    if let Err(e) = attached {
        give_up(&scheduling_status_tx, format!("Overlay window setup could not be scheduled: {e}"));
    }
}

fn give_up(gui_status_tx: &mpsc::Sender<GuiStatus>, reason: String) {
    error!("{reason}");
    let _ = gui_status_tx.try_send(GuiStatus::Error(reason));
    let _ = gui_status_tx.try_send(GuiStatus::ShuttingDown);
}

fn ns_window(winit_window: &Window) -> Option<Retained<NSWindow>> {
    let RawWindowHandle::AppKit(handle) = winit_window.window_handle().ok()?.as_raw() else {
        return None;
    };
    let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
    view.window()
}

fn configure(window: &NSWindow) {
    window.orderOut(None);
    window.setStyleMask(NSWindowStyleMask::Borderless);
    window.setContentSize(NSSize::new(OVERLAY_SIZE.width, OVERLAY_SIZE.height));
    window.setLevel(NSStatusWindowLevel);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    window.setIgnoresMouseEvents(true);
}

fn place(window: &NSWindow, mtm: MainThreadMarker) {
    let Some(screen) = NSScreen::mainScreen(mtm) else {
        return;
    };
    let free = screen.visibleFrame();
    let origin = bottom_center(
        Rect {
            x: free.origin.x,
            y: free.origin.y,
            width: free.size.width,
            height: free.size.height,
        },
        OVERLAY_SIZE,
        BOTTOM_MARGIN,
    );
    let current = window.frame().origin;
    if current.x != origin.x || current.y != origin.y {
        window.setFrameOrigin(NSPoint::new(origin.x, origin.y));
    }
}

fn tick() {
    OVERLAY.with_borrow_mut(|overlay| {
        if let Some(overlay) = overlay {
            overlay.tick();
        }
    });
}

fn hide_if_idle() {
    OVERLAY.with_borrow_mut(|overlay| {
        let Some(overlay) = overlay else {
            return;
        };
        let idle = overlay.shared_state.read().is_ok_and(|state| !overlay_visible(state.gui_state));
        if let (true, Some(window)) = (idle, &overlay.window) {
            window.orderOut(None);
            overlay.ordered_in = false;
        }
    });
}

impl Overlay {
    fn tick(&mut self) {
        self.clock.wake.acknowledge();

        if self.reload_flag.load(Ordering::SeqCst) {
            info!("UI file changed, exiting so the service manager restarts the daemon");
            std::process::exit(EXIT_CODE_RELOAD);
        }

        let Ok(state) = self.shared_state.read() else {
            return;
        };
        let gui_state = state.gui_state;
        let spinner_angle = self.spinner.advance(gui_state, Instant::now());
        apply_state(&self.instance, &state, true, spinner_angle);
        drop(state);

        let (Some(window), Some(mtm)) = (&self.window, MainThreadMarker::new()) else {
            return;
        };
        if overlay_visible(gui_state) {
            self.hide_timer.stop();
            place(window, mtm);
            if !self.ordered_in {
                debug!("Overlay shown for {:?}", gui_state);
                window.orderFrontRegardless();
                self.ordered_in = true;
            }
        } else if self.ordered_in && !self.hide_timer.running() {
            self.hide_timer.start(slint::TimerMode::SingleShot, HIDE_DELAY, hide_if_idle);
        }
    }
}
