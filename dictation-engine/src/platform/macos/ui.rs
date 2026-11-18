use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use tokio::sync::{broadcast, mpsc};
use tracing::warn;

use crate::{GuiControl, GuiStatus};

const MAIN_THREAD_STACK_BYTES: usize = 8 * 1024 * 1024;

static UI_LOOP: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "tray")]
pub(super) fn ui_loop_running() -> bool {
    UI_LOOP.load(Ordering::Acquire)
}

struct QuitUiOnDrop;

impl Drop for QuitUiOnDrop {
    fn drop(&mut self) {
        slint_gui::quit();
    }
}

pub fn run_daemon<F, Fut>(daemon: F) -> Result<()>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>>,
{
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to build the tokio runtime")?;

    if let Err(e) = slint_gui::init() {
        warn!("No UI event loop ({e}); running without overlay and tray");
        return runtime.block_on(daemon());
    }
    UI_LOOP.store(true, Ordering::Release);

    let daemon_thread = std::thread::Builder::new()
        .name("daemon".into())
        .stack_size(MAIN_THREAD_STACK_BYTES)
        .spawn(move || {
            let _quit_ui = QuitUiOnDrop;
            runtime.block_on(daemon())
        })
        .context("Failed to start the daemon thread")?;

    if let Err(e) = slint_gui::run_until_quit() {
        warn!("UI event loop stopped: {e}");
    }
    UI_LOOP.store(false, Ordering::Release);

    match daemon_thread.join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

pub fn spawn_overlay(
    gui_control_tx: broadcast::Sender<GuiControl>,
    spectrum_tx: broadcast::Sender<Vec<f32>>,
    gui_status_tx: mpsc::Sender<GuiStatus>,
) -> bool {
    if !UI_LOOP.load(Ordering::Acquire) {
        return false;
    }
    slint_gui::attach_overlay(
        gui_control_tx,
        spectrum_tx,
        gui_status_tx,
        tokio::runtime::Handle::current(),
    );
    true
}
