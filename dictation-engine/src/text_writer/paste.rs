//! Clipboard-paste writer: snapshot → `wl-copy` → Ctrl+V → restore.
//!
//! The snapshot reads every MIME type the current selection offers
//! (`wl-paste --list-types`, then `wl-paste -n -t <type>` per type). The
//! restore offers all of them again through `zwlr_data_control_v1`
//! ([`super::clipboard_owner`]); without that protocol it falls back to
//! `wl-copy` of the single most useful type.

use super::clipboard_owner::{self, Snapshot};
use super::WriteError;
use anyhow::{Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

/// MIME type the dictation text is offered as. wl-copy adds the usual text
/// aliases (`text/plain`, `UTF8_STRING`, `STRING`, ...) on its own.
const TEXT_MIME: &str = "text/plain;charset=utf-8";

/// X11 selection meta-targets XWayland sources advertise; not real data.
const META_TARGETS: &[&str] = &["TARGETS", "MULTIPLE", "TIMESTAMP", "SAVE_TARGETS", "DELETE"];

/// Per-`wl-paste` read timeout; a wedged source must not stall dictation.
const READ_TIMEOUT: Duration = Duration::from_secs(1);

/// How many times to read back the selection after `wl-copy` before giving
/// up (wl-copy daemonizes, so its exit does not prove the selection is set).
const SET_VERIFY_TRIES: u32 = 15;
const SET_VERIFY_INTERVAL: Duration = Duration::from_millis(20);

/// The clipboard and keystroke operations the paste writer needs. The live
/// implementation shells out to wl-clipboard and wtype; tests use a fake.
pub(super) trait Clipboard {
    /// Offered MIME types, or `None` when the clipboard is empty.
    async fn list_types(&mut self) -> Result<Option<Vec<String>>>;
    async fn read(&mut self, mime: &str) -> Result<Vec<u8>>;
    /// Set the selection to `data`, offered as `mime` (wl-copy's default
    /// type inference when `None`).
    async fn set(&mut self, mime: Option<&str>, data: &[u8]) -> Result<()>;
    async fn clear(&mut self) -> Result<()>;
    /// Put a multi-type snapshot back.
    async fn restore(&mut self, snapshot: Snapshot) -> Result<()>;
    async fn send_paste(&mut self, terminal: bool) -> Result<()>;
    async fn sleep(&mut self, d: Duration);
}

/// Types worth snapshotting: everything but X11 meta-targets.
fn snapshot_types(types: &[String]) -> Vec<&str> {
    types.iter().map(String::as_str).filter(|t| !META_TARGETS.contains(t)).collect()
}

/// The single type to restore when only `wl-copy` is available: UTF-8 text
/// first, then any text type, then whatever the source offered first.
fn best_single_type(snapshot: &Snapshot) -> Option<usize> {
    let pos = |pred: &dyn Fn(&str) -> bool| snapshot.iter().position(|(m, _)| pred(m));
    pos(&|m| m == TEXT_MIME || m == "UTF8_STRING")
        .or_else(|| pos(&|m| m == "text/plain"))
        .or_else(|| pos(&|m| m.starts_with("text/")))
        .or(if snapshot.is_empty() { None } else { Some(0) })
}

/// wtype arguments for the paste chord.
fn paste_keystroke_args(terminal: bool) -> Vec<&'static str> {
    if terminal {
        vec!["-M", "ctrl", "-M", "shift", "-k", "v", "-m", "shift", "-m", "ctrl"]
    } else {
        vec!["-M", "ctrl", "-k", "v", "-m", "ctrl"]
    }
}

async fn take_snapshot<C: Clipboard>(c: &mut C) -> Result<Snapshot> {
    let Some(types) = c.list_types().await? else {
        return Ok(Vec::new());
    };
    let wanted = snapshot_types(&types);
    let mut snapshot = Vec::new();
    for &mime in &wanted {
        match c.read(mime).await {
            Ok(bytes) => snapshot.push((mime.to_string(), bytes)),
            Err(e) => debug!("clipboard snapshot: skipping {}: {}", mime, e),
        }
    }
    if snapshot.is_empty() && !wanted.is_empty() {
        anyhow::bail!("clipboard offers {} type(s) but none could be read", wanted.len());
    }
    Ok(snapshot)
}

/// Wait until the selection holds `text` (wl-copy has taken ownership).
async fn wait_for_selection<C: Clipboard>(c: &mut C, text: &str) -> bool {
    for _ in 0..SET_VERIFY_TRIES {
        if c.read(TEXT_MIME).await.is_ok_and(|b| b == text.as_bytes()) {
            return true;
        }
        c.sleep(SET_VERIFY_INTERVAL).await;
    }
    false
}

async fn put_back<C: Clipboard>(c: &mut C, snapshot: Snapshot) -> Result<()> {
    if snapshot.is_empty() {
        return c.clear().await;
    }
    c.restore(snapshot).await
}

/// Paste `text` into the focused window.
///
/// With `restore` set, the user's clipboard is snapshotted first and put back
/// `restore_delay` after the keystroke — unless something else took the
/// clipboard in the meantime (the user copied during the delay), in which
/// case it is left alone.
pub(super) async fn paste<C: Clipboard>(
    c: &mut C,
    text: &str,
    terminal: bool,
    restore: bool,
    restore_delay: Duration,
) -> Result<(), WriteError> {
    let unavailable =
        |what: &str, e: anyhow::Error| WriteError::Unavailable(format!("{what}: {e:#}"));

    // Refuse to paste if the clipboard can't be snapshotted: it could not be
    // restored afterwards.
    let snapshot = if restore {
        Some(take_snapshot(c).await.map_err(|e| unavailable("cannot snapshot clipboard", e))?)
    } else {
        None
    };

    c.set(Some(TEXT_MIME), text.as_bytes()).await.map_err(|e| unavailable("wl-copy failed", e))?;

    if !wait_for_selection(c, text).await {
        if let Some(snapshot) = snapshot {
            let _ = put_back(c, snapshot).await;
        }
        return Err(WriteError::Unavailable(
            "clipboard never reported the dictation text after wl-copy".into(),
        ));
    }

    if let Err(e) = c.send_paste(terminal).await {
        if let Some(snapshot) = snapshot {
            let _ = put_back(c, snapshot).await;
        }
        return Err(unavailable("paste keystroke failed", e));
    }

    // The text is pasted (or at least the keystroke was delivered); restore
    // problems are warnings from here on, never a reason to fall through.
    if let Some(snapshot) = snapshot {
        c.sleep(restore_delay).await;
        let still_ours = c.read(TEXT_MIME).await.is_ok_and(|b| b == text.as_bytes());
        if !still_ours {
            info!("Clipboard changed during paste; leaving the new contents in place");
        } else {
            let types = snapshot.len();
            match put_back(c, snapshot).await {
                Ok(()) => debug!("Restored clipboard snapshot ({} type(s))", types),
                Err(e) => warn!("Failed to restore clipboard: {:#}", e),
            }
        }
    }
    Ok(())
}

/// `wl-copy`, feeding `data` on stdin (binary-safe, and text starting with
/// `-` is never parsed as an option).
pub(super) async fn wl_copy(mime: Option<&str>, data: &[u8]) -> Result<()> {
    let mut cmd = tokio::process::Command::new("wl-copy");
    if let Some(mime) = mime {
        cmd.args(["--type", mime]);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        // Not piped: the forked child that serves the selection would hold
        // the pipe open and a read would never see EOF.
        .stderr(Stdio::null())
        .spawn()
        .context("cannot run wl-copy")?;
    let mut stdin = child.stdin.take().context("wl-copy stdin")?;
    stdin.write_all(data).await?;
    drop(stdin);
    // wl-copy forks to serve the selection; the parent exits once it is set up.
    let status =
        tokio::time::timeout(READ_TIMEOUT, child.wait()).await.context("wl-copy timed out")??;
    anyhow::ensure!(status.success(), "wl-copy exited with {status}");
    Ok(())
}

/// wl-clipboard + wtype.
pub(super) struct SystemClipboard;

impl SystemClipboard {
    async fn wl_paste(args: &[&str]) -> Result<std::process::Output> {
        let out = tokio::time::timeout(
            READ_TIMEOUT,
            tokio::process::Command::new("wl-paste")
                .args(args)
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("wl-paste timed out")?
        .context("cannot run wl-paste")?;
        Ok(out)
    }
}

impl Clipboard for SystemClipboard {
    async fn list_types(&mut self) -> Result<Option<Vec<String>>> {
        let out = Self::wl_paste(&["--list-types"]).await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // "Nothing is copied" (empty clipboard) / "No selection".
            if stderr.contains("Nothing is copied") || stderr.contains("No selection") {
                return Ok(None);
            }
            anyhow::bail!("wl-paste --list-types: {}", stderr.trim());
        }
        let types: Vec<String> =
            String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect();
        Ok(if types.is_empty() { None } else { Some(types) })
    }

    async fn read(&mut self, mime: &str) -> Result<Vec<u8>> {
        let out = Self::wl_paste(&["--no-newline", "--type", mime]).await?;
        if !out.status.success() {
            anyhow::bail!(
                "wl-paste --type {}: {}",
                mime,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }

    async fn set(&mut self, mime: Option<&str>, data: &[u8]) -> Result<()> {
        wl_copy(mime, data).await
    }

    async fn clear(&mut self) -> Result<()> {
        let status = tokio::process::Command::new("wl-copy")
            .arg("--clear")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;
        anyhow::ensure!(status.success(), "wl-copy --clear failed");
        Ok(())
    }

    async fn restore(&mut self, snapshot: Snapshot) -> Result<()> {
        let fallback = best_single_type(&snapshot).map(|i| snapshot[i].clone());
        let types = snapshot.len();
        match tokio::task::spawn_blocking(move || clipboard_owner::restore(snapshot)).await? {
            Ok(()) => Ok(()),
            Err(e) => {
                let Some((mime, bytes)) = fallback else { return Ok(()) };
                info!(
                    "Full clipboard restore unavailable ({}); restoring {} only (of {} type(s))",
                    e, mime, types
                );
                wl_copy(Some(&mime), &bytes).await
            }
        }
    }

    async fn send_paste(&mut self, terminal: bool) -> Result<()> {
        let out = tokio::process::Command::new("wtype")
            .args(paste_keystroke_args(terminal))
            .stdin(Stdio::null())
            .output()
            .await
            .context("cannot run wtype")?;
        if !out.status.success() {
            anyhow::bail!("wtype: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    async fn sleep(&mut self, d: Duration) {
        tokio::time::sleep(d).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory clipboard. `steal_after_paste` simulates the user copying
    /// something during the restore delay.
    #[derive(Default)]
    struct FakeClipboard {
        contents: Snapshot,
        log: Vec<String>,
        fail_paste: bool,
        set_is_ignored: bool,
        steal_after_paste: Option<Snapshot>,
        restored: Option<Snapshot>,
    }

    impl Clipboard for FakeClipboard {
        async fn list_types(&mut self) -> Result<Option<Vec<String>>> {
            self.log.push("list".into());
            Ok((!self.contents.is_empty())
                .then(|| self.contents.iter().map(|(m, _)| m.clone()).collect()))
        }
        async fn read(&mut self, mime: &str) -> Result<Vec<u8>> {
            self.contents
                .iter()
                .find(|(m, _)| m == mime)
                .map(|(_, b)| b.clone())
                .ok_or_else(|| anyhow::anyhow!("no {mime}"))
        }
        async fn set(&mut self, mime: Option<&str>, data: &[u8]) -> Result<()> {
            self.log.push(format!("set {}", mime.unwrap_or("-")));
            if !self.set_is_ignored {
                self.contents = vec![(mime.unwrap_or(TEXT_MIME).to_string(), data.to_vec())];
            }
            Ok(())
        }
        async fn clear(&mut self) -> Result<()> {
            self.log.push("clear".into());
            self.contents.clear();
            Ok(())
        }
        async fn restore(&mut self, snapshot: Snapshot) -> Result<()> {
            self.log.push("restore".into());
            self.contents = snapshot.clone();
            self.restored = Some(snapshot);
            Ok(())
        }
        async fn send_paste(&mut self, terminal: bool) -> Result<()> {
            self.log.push(format!("paste terminal={terminal}"));
            if self.fail_paste {
                anyhow::bail!("wtype missing");
            }
            if let Some(stolen) = self.steal_after_paste.take() {
                self.contents = stolen;
            }
            Ok(())
        }
        async fn sleep(&mut self, _d: Duration) {}
    }

    fn snap(items: &[(&str, &str)]) -> Snapshot {
        items.iter().map(|(m, b)| (m.to_string(), b.as_bytes().to_vec())).collect()
    }

    const DELAY: Duration = Duration::from_millis(500);

    #[tokio::test]
    async fn restores_every_snapshotted_type() {
        let original = snap(&[
            ("text/uri-list", "file:///tmp/a"),
            ("x-special/gnome-copied-files", "copy\nfile:///tmp/a"),
            ("text/plain", "/tmp/a"),
            ("TARGETS", "junk"),
        ]);
        let mut c = FakeClipboard { contents: original.clone(), ..Default::default() };
        paste(&mut c, "hello world", false, true, DELAY).await.unwrap();
        // Meta-target dropped, everything else restored in source order.
        assert_eq!(c.restored, Some(original[..3].to_vec()));
        assert_eq!(
            c.log,
            vec!["list", "set text/plain;charset=utf-8", "paste terminal=false", "restore"]
        );
    }

    #[tokio::test]
    async fn empty_clipboard_is_cleared_after_paste() {
        let mut c = FakeClipboard::default();
        paste(&mut c, "hi", true, true, DELAY).await.unwrap();
        assert!(c.contents.is_empty());
        assert_eq!(c.log.last().unwrap(), "clear");
        assert!(c.log.contains(&"paste terminal=true".to_string()));
    }

    #[tokio::test]
    async fn user_copy_during_delay_is_not_clobbered() {
        let stolen = snap(&[(TEXT_MIME, "user copied this")]);
        let mut c = FakeClipboard {
            contents: snap(&[(TEXT_MIME, "old")]),
            steal_after_paste: Some(stolen.clone()),
            ..Default::default()
        };
        paste(&mut c, "dictated", false, true, DELAY).await.unwrap();
        assert_eq!(c.contents, stolen);
        assert_eq!(c.restored, None);
    }

    #[tokio::test]
    async fn keep_in_clipboard_skips_snapshot_and_restore() {
        let mut c = FakeClipboard { contents: snap(&[(TEXT_MIME, "old")]), ..Default::default() };
        paste(&mut c, "dictated", false, false, DELAY).await.unwrap();
        assert_eq!(c.contents, snap(&[(TEXT_MIME, "dictated")]));
        assert!(!c.log.contains(&"list".to_string()));
        assert_eq!(c.restored, None);
    }

    #[tokio::test]
    async fn keystroke_failure_restores_and_falls_through() {
        let original = snap(&[(TEXT_MIME, "old")]);
        let mut c =
            FakeClipboard { contents: original.clone(), fail_paste: true, ..Default::default() };
        let err = paste(&mut c, "dictated", false, true, DELAY).await.unwrap_err();
        assert!(matches!(err, WriteError::Unavailable(ref m) if m.contains("paste keystroke")));
        assert_eq!(c.contents, original);
    }

    #[tokio::test]
    async fn selection_never_taken_falls_through_without_pasting() {
        let original = snap(&[(TEXT_MIME, "old")]);
        let mut c = FakeClipboard {
            contents: original.clone(),
            set_is_ignored: true,
            ..Default::default()
        };
        let err = paste(&mut c, "dictated", false, true, DELAY).await.unwrap_err();
        assert!(matches!(err, WriteError::Unavailable(_)));
        assert!(!c.log.iter().any(|l| l.starts_with("paste")));
        assert_eq!(c.contents, original);
    }

    #[test]
    fn meta_targets_are_not_snapshotted() {
        let types: Vec<String> =
            ["TARGETS", "text/plain", "SAVE_TARGETS", "image/png", "TIMESTAMP"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        assert_eq!(snapshot_types(&types), vec!["text/plain", "image/png"]);
    }

    #[test]
    fn best_single_type_prefers_utf8_text() {
        let s = snap(&[("image/png", "x"), ("text/html", "<b>"), ("UTF8_STRING", "b")]);
        assert_eq!(best_single_type(&s), Some(2));
        let s = snap(&[("image/png", "x"), ("text/html", "<b>")]);
        assert_eq!(best_single_type(&s), Some(1));
        let s = snap(&[("image/png", "x")]);
        assert_eq!(best_single_type(&s), Some(0));
        assert_eq!(best_single_type(&Vec::new()), None);
    }

    #[test]
    fn terminals_paste_with_ctrl_shift_v() {
        assert_eq!(paste_keystroke_args(false), vec!["-M", "ctrl", "-k", "v", "-m", "ctrl"]);
        assert_eq!(
            paste_keystroke_args(true),
            vec!["-M", "ctrl", "-M", "shift", "-k", "v", "-m", "shift", "-m", "ctrl"]
        );
    }
}
