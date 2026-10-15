//! `zwp_input_method_v2` writer: `commit_string` + `commit` into the focused
//! text input.
//!
//! The input-method slot is held only for the duration of one commit: bind,
//! wait for the compositor to `activate` us on the focused text input, commit,
//! round-trip, destroy. This is the lifecycle validated live in
//! `docs/ime-backend-findings.md`, and it means a user-run IME (fcitx5/ibus)
//! is only ever displaced while the slot is free anyway — if one owns it, the
//! compositor answers `unavailable` and we fall through without retrying.
//!
//! Never calls `grab_keyboard`.

use super::WriteError;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use wayland_client::{
    delegate_noop,
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_registry, wl_seat::WlSeat},
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwp_input_method_v2::{Event as ImeEvent, ZwpInputMethodV2},
};

/// The subset of input-method events the writer acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
    Activate,
    Deactivate,
    Done,
    Unavailable,
}

/// Double-buffered input-method state, applied on each `done`.
///
/// Hyprland sends an initial `done` *before* `activate` on bind (findings,
/// quirk 1), so readiness is "a `done` has applied an `activate`", never just
/// "a `done` arrived".
#[derive(Debug, Default)]
struct Tracker {
    pending_activate: bool,
    pending_deactivate: bool,
    active: bool,
    /// Number of `done` events received; the serial `commit` must carry.
    done_count: u32,
    unavailable: bool,
}

impl Tracker {
    fn feed(&mut self, signal: Signal) {
        match signal {
            Signal::Activate => self.pending_activate = true,
            Signal::Deactivate => self.pending_deactivate = true,
            Signal::Unavailable => self.unavailable = true,
            Signal::Done => {
                if self.pending_activate {
                    self.active = true;
                }
                if self.pending_deactivate {
                    self.active = false;
                }
                self.pending_activate = false;
                self.pending_deactivate = false;
                self.done_count += 1;
            }
        }
    }

    /// The serial to commit with, once a text input is active.
    fn ready(&self) -> Option<u32> {
        (self.active && !self.unavailable).then_some(self.done_count)
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Tracker {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(Tracker: ignore WlSeat);
delegate_noop!(Tracker: ZwpInputMethodManagerV2);

impl Dispatch<ZwpInputMethodV2, ()> for Tracker {
    fn event(
        state: &mut Self,
        _: &ZwpInputMethodV2,
        event: ImeEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ImeEvent::Activate => state.feed(Signal::Activate),
            ImeEvent::Deactivate => state.feed(Signal::Deactivate),
            ImeEvent::Done => state.feed(Signal::Done),
            ImeEvent::Unavailable => state.feed(Signal::Unavailable),
            _ => {}
        }
    }
}

/// How a slot session ended.
#[derive(Debug, PartialEq, Eq)]
enum SessionEnd {
    /// Text was committed (only when a commit was requested).
    Committed,
    /// A text input was active; nothing was committed (probe mode).
    Active,
    /// Another input method owns the slot.
    Unavailable,
    /// The deadline passed without an active text input.
    NoActiveInput,
}

/// Errors before the slot is bound: nothing can have been written.
fn unavailable(msg: impl Into<String>) -> WriteError {
    WriteError::Unavailable(msg.into())
}

/// Bind the input-method slot until it is active (then optionally commit),
/// `unavailable` arrives, or `timeout` passes. The slot is released before
/// returning.
fn session(commit_text: Option<&str>, timeout: Duration) -> Result<SessionEnd, WriteError> {
    let conn = Connection::connect_to_env()
        .map_err(|e| unavailable(format!("no wayland display: {e}")))?;
    let (globals, mut queue) = registry_queue_init::<Tracker>(&conn)
        .map_err(|e| unavailable(format!("wayland registry init failed: {e}")))?;
    let qh = queue.handle();
    let seat: WlSeat = globals
        .bind(&qh, 1..=4, ())
        .map_err(|e| unavailable(format!("cannot bind wl_seat: {e}")))?;
    let manager: ZwpInputMethodManagerV2 = globals.bind(&qh, 1..=1, ()).map_err(|_| {
        unavailable("compositor does not expose zwp_input_method_manager_v2".to_string())
    })?;

    let mut state = Tracker::default();
    let input_method = manager.get_input_method(&seat, &qh, ());
    let io_err = |e: &dyn std::fmt::Display| unavailable(format!("wayland I/O error: {e}"));
    queue.flush().map_err(|e| io_err(&e))?;

    let deadline = Instant::now() + timeout;
    let end = loop {
        queue.dispatch_pending(&mut state).map_err(|e| io_err(&e))?;
        if state.unavailable {
            break SessionEnd::Unavailable;
        }
        if let Some(serial) = state.ready() {
            let Some(text) = commit_text else {
                break SessionEnd::Active;
            };
            input_method.commit_string(text.to_string());
            input_method.commit(serial);
            // From here on the text may already be in the target: errors are
            // Failed, never Unavailable, so the chain does not insert twice.
            queue.roundtrip(&mut state).map_err(|e| {
                WriteError::Failed(anyhow::anyhow!("wayland round-trip after commit failed: {e}"))
            })?;
            break SessionEnd::Committed;
        }
        let now = Instant::now();
        if now >= deadline {
            break SessionEnd::NoActiveInput;
        }
        queue.flush().map_err(|e| io_err(&e))?;
        // prepare_read is None when events are already queued; loop again.
        if let Some(guard) = queue.prepare_read() {
            let mut pfd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout_ms = (deadline - now).as_millis().clamp(1, 50) as i32;
            // SAFETY: pfd is a valid pollfd for the duration of the call.
            if unsafe { libc::poll(&mut pfd, 1, timeout_ms) } > 0 {
                let _ = guard.read();
            }
        }
    };

    input_method.destroy();
    let _ = queue.flush();
    Ok(end)
}

/// Commit `text` into the focused text input.
///
/// Returns [`WriteError::Unavailable`] when nothing was written (no display,
/// no input-method manager, another IME owns the slot, or no text input
/// activated within `activate_timeout`).
pub fn commit(text: &str, activate_timeout: Duration) -> Result<(), WriteError> {
    check_length(text)?;
    match session(Some(text), activate_timeout)? {
        SessionEnd::Committed => Ok(()),
        SessionEnd::Unavailable => Err(unavailable(
            "another input method (fcitx5/ibus?) owns the zwp_input_method_v2 slot",
        )),
        SessionEnd::NoActiveInput | SessionEnd::Active => Err(unavailable(format!(
            "focused window activated no text input within {} ms \
             (no text-input-v3 support, e.g. XWayland or Electron without --enable-wayland-ime)",
            activate_timeout.as_millis()
        ))),
    }
}

/// Conservative ceiling for one `commit_string`. A Wayland message has
/// historically been capped at 4096 bytes, and the compositor forwards the
/// string to the client as one `zwp_text_input_v3.commit_string`; longer
/// dictations go to the paste writer instead. Raise once the real limit is
/// confirmed live.
const MAX_COMMIT_BYTES: usize = 3500;

fn check_length(text: &str) -> Result<(), WriteError> {
    if text.len() > MAX_COMMIT_BYTES {
        return Err(unavailable(format!(
            "text too long for one commit_string ({} bytes > {MAX_COMMIT_BYTES})",
            text.len()
        )));
    }
    Ok(())
}

/// Input-method slot state, for `diagnose`.
#[derive(Debug, PartialEq, Eq)]
pub enum SlotStatus {
    /// Slot free and a text input activated us (diagnose's own terminal, usually).
    FreeActive,
    /// Slot free; nothing focused activated a text input.
    Free,
    /// Another input method owns the slot.
    Taken,
    /// The slot could not be probed (no display, no manager, ...).
    Unusable(String),
}

impl std::fmt::Display for SlotStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SlotStatus::FreeActive => {
                f.write_str("input-method slot free; focused window activated a text input")
            }
            SlotStatus::Free => {
                f.write_str("input-method slot free (focused window did not activate a text input)")
            }
            SlotStatus::Taken => {
                f.write_str("UNAVAILABLE: another input method owns the slot — falls through")
            }
            SlotStatus::Unusable(why) => write!(f, "UNUSABLE: {why}"),
        }
    }
}

/// Briefly bind the slot to see whether the IME writer could use it. Never
/// commits anything.
pub fn probe_slot() -> SlotStatus {
    match session(None, Duration::from_millis(300)) {
        Ok(SessionEnd::Active | SessionEnd::Committed) => SlotStatus::FreeActive,
        Ok(SessionEnd::NoActiveInput) => SlotStatus::Free,
        Ok(SessionEnd::Unavailable) => SlotStatus::Taken,
        Err(WriteError::Unavailable(why)) => SlotStatus::Unusable(why),
        Err(WriteError::Failed(e)) => SlotStatus::Unusable(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(signals: &[Signal]) -> Tracker {
        let mut t = Tracker::default();
        for &s in signals {
            t.feed(s);
        }
        t
    }

    #[test]
    fn initial_done_before_activate_is_not_ready() {
        // Hyprland's first batch on bind: done with no activate.
        assert_eq!(feed(&[Signal::Done]).ready(), None);
    }

    #[test]
    fn activate_is_ready_only_after_its_done() {
        assert_eq!(feed(&[Signal::Done, Signal::Activate]).ready(), None);
        // Serial = number of done events received.
        assert_eq!(feed(&[Signal::Done, Signal::Activate, Signal::Done]).ready(), Some(2));
    }

    #[test]
    fn deactivate_clears_readiness() {
        let t = feed(&[Signal::Activate, Signal::Done, Signal::Deactivate, Signal::Done]);
        assert_eq!(t.ready(), None);
        assert_eq!(t.done_count, 2);
    }

    #[test]
    fn unavailable_is_never_ready() {
        let t = feed(&[Signal::Activate, Signal::Done, Signal::Unavailable]);
        assert!(t.unavailable);
        assert_eq!(t.ready(), None);
    }

    #[test]
    fn long_text_falls_through_before_binding() {
        assert!(check_length(&"a".repeat(MAX_COMMIT_BYTES)).is_ok());
        let err = check_length(&"a".repeat(MAX_COMMIT_BYTES + 1)).unwrap_err();
        assert!(matches!(err, WriteError::Unavailable(ref m) if m.contains("too long")));
    }

    #[test]
    fn slot_status_messages_flag_problems() {
        assert!(SlotStatus::Taken.to_string().starts_with("UNAVAILABLE"));
        assert!(SlotStatus::Unusable("x".into()).to_string().starts_with("UNUSABLE"));
        assert!(SlotStatus::Free.to_string().contains("free"));
    }
}
