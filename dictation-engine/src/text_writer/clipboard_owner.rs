//! Restores a multi-type clipboard snapshot through `zwlr_data_control_v1`.
//!
//! `wl-copy` offers one MIME type per invocation, so it cannot put back a
//! clipboard that offered several (a file-manager copy offers
//! `text/uri-list` + `x-special/gnome-copied-files` + `text/plain`; a browser
//! image copy offers `image/png` + `text/html`). Instead a thread owns the
//! selection, offers every snapshotted type, and serves `send` requests until
//! another client takes the selection (`cancelled`).
//!
//! Unlike wl-copy's forked child, this owner lives in the daemon: a daemon
//! restart empties a clipboard it restored.

use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use wayland_client::{
    delegate_noop, event_created_child,
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_registry, wl_seat::WlSeat},
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

/// Clipboard contents: (MIME type, bytes), in the order the source offered
/// them.
pub(super) type Snapshot = Vec<(String, Vec<u8>)>;

/// Bytes to serve for a `send(mime)` request.
fn lookup<'a>(snapshot: &'a Snapshot, mime: &str) -> Option<&'a [u8]> {
    snapshot.iter().find(|(m, _)| m == mime).map(|(_, b)| b.as_slice())
}

struct Owner {
    snapshot: Arc<Snapshot>,
    cancelled: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Owner {
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

delegate_noop!(Owner: ignore WlSeat);
delegate_noop!(Owner: ZwlrDataControlManagerV1);
delegate_noop!(Owner: ignore ZwlrDataControlOfferV1);

impl Dispatch<ZwlrDataControlDeviceV1, ()> for Owner {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // Offers for the current selection (usually our own); not needed.
            zwlr_data_control_device_v1::Event::Selection { id: Some(offer) } => offer.destroy(),
            zwlr_data_control_device_v1::Event::Finished => state.cancelled = true,
            _ => {}
        }
    }

    event_created_child!(Owner, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for Owner {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                serve(Arc::clone(&state.snapshot), mime_type, fd);
            }
            zwlr_data_control_source_v1::Event::Cancelled => state.cancelled = true,
            _ => {}
        }
    }
}

/// Write the bytes for `mime` to `fd` on its own thread, so a slow reader
/// never stalls the dispatch loop. Dropping the fd closes it (EOF).
fn serve(snapshot: Arc<Snapshot>, mime: String, fd: OwnedFd) {
    std::thread::spawn(move || {
        let mut file = std::fs::File::from(fd);
        if let Some(bytes) = lookup(&snapshot, &mime) {
            if let Err(e) = file.write_all(bytes) {
                tracing::debug!("clipboard restore: writing {} failed: {}", mime, e);
            }
        }
    });
}

/// Whether the compositor exposes `zwlr_data_control_manager_v1`.
pub(super) fn data_control_available() -> anyhow::Result<bool> {
    let conn = Connection::connect_to_env()?;
    let (globals, _queue) = registry_queue_init::<Owner>(&conn)?;
    Ok(globals
        .contents()
        .with_list(|list| list.iter().any(|g| g.interface == "zwlr_data_control_manager_v1")))
}

/// Take the clipboard selection and offer every type in `snapshot`.
///
/// Returns once the selection is set; a background thread keeps serving it
/// until another client replaces it. Errors (no display, no data-control
/// global) mean the selection was not touched.
pub(super) fn restore(snapshot: Snapshot) -> anyhow::Result<()> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<Owner>(&conn)?;
    let qh = queue.handle();
    let seat: WlSeat = globals.bind(&qh, 1..=4, ())?;
    // v1: no primary-selection events; we only ever touch the clipboard.
    let manager: ZwlrDataControlManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| anyhow::anyhow!("compositor does not expose zwlr_data_control_manager_v1"))?;

    let source = manager.create_data_source(&qh, ());
    for (mime, _) in &snapshot {
        source.offer(mime.clone());
    }
    let device = manager.get_data_device(&seat, &qh, ());
    device.set_selection(Some(&source));

    let mut owner = Owner { snapshot: Arc::new(snapshot), cancelled: false };
    queue.roundtrip(&mut owner)?;

    std::thread::Builder::new().name("clipboard-restore".into()).spawn(move || {
        while !owner.cancelled {
            if let Err(e) = queue.blocking_dispatch(&mut owner) {
                tracing::debug!("clipboard restore owner exiting: {}", e);
                break;
            }
        }
        source.destroy();
        device.destroy();
        manager.destroy();
        let _ = queue.flush();
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_serves_exact_mime() {
        let snap: Snapshot = vec![
            ("text/uri-list".into(), b"file:///tmp/a".to_vec()),
            ("text/plain".into(), b"/tmp/a".to_vec()),
        ];
        assert_eq!(lookup(&snap, "text/plain"), Some(&b"/tmp/a"[..]));
        assert_eq!(lookup(&snap, "text/uri-list"), Some(&b"file:///tmp/a"[..]));
        assert_eq!(lookup(&snap, "image/png"), None);
    }
}
