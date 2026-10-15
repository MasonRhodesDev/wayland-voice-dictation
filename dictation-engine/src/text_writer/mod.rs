//! Text writer chain: how the final transcript reaches the focused window.
//!
//! Writers are tried in order, falling through to the next one when a writer
//! reports that it could not write anything:
//!
//! 1. [`WriterKind::Ime`] — `zwp_input_method_v2` `commit_string`. One atomic
//!    commit, no clipboard involvement. Falls through when the compositor has
//!    no input-method manager, another IME (fcitx5/ibus) owns the slot
//!    (`unavailable`), or the focused client never activates a text-input-v3
//!    field (XWayland, Electron without `--enable-wayland-ime`, ...).
//! 2. [`WriterKind::Paste`] — snapshot the clipboard (every MIME type),
//!    `wl-copy` the text, send Ctrl+V (Ctrl+Shift+V in terminals), then
//!    restore the snapshot.
//! 3. [`WriterKind::Wtype`] — the original per-key `wtype` injection.
//!
//! A writer that *may* have written something (failure after the IME commit
//! or after the paste keystroke, or any wtype failure) stops the chain instead
//! of falling through, so the text is never inserted twice.

mod clipboard_owner;
pub mod ime;
mod paste;

use crate::keyboard::KeyboardInjector;
use crate::window_detect::AppCategory;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, warn};

/// How long the IME writer waits for the focused client to activate a text
/// input before falling through. Hyprland delivers `activate` ~10 ms after
/// the slot is bound when a text field is focused.
const IME_ACTIVATE_TIMEOUT: Duration = Duration::from_millis(300);

/// One tier of the writer chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WriterKind {
    Ime,
    Paste,
    Wtype,
}

impl WriterKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WriterKind::Ime => "ime",
            WriterKind::Paste => "paste",
            WriterKind::Wtype => "wtype",
        }
    }
}

impl std::fmt::Display for WriterKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The full chain, in fallback order.
pub const DEFAULT_CHAIN: [WriterKind; 3] = [WriterKind::Ime, WriterKind::Paste, WriterKind::Wtype];

/// Writer chain for the `text_writer` config value: the chain starts at the
/// named writer and keeps the tiers below it. Unknown values use the full
/// chain.
pub fn chain_for(preferred: &str) -> &'static [WriterKind] {
    match preferred.trim().to_lowercase().as_str() {
        "ime" | "auto" | "" => &DEFAULT_CHAIN,
        "paste" => &DEFAULT_CHAIN[1..],
        "wtype" => &DEFAULT_CHAIN[2..],
        other => {
            warn!("Unknown text_writer '{}', using the full ime → paste → wtype chain", other);
            &DEFAULT_CHAIN
        }
    }
}

/// Writer chain for one injection. Terminals get wtype with its per-word
/// delay, the pre-chain behaviour: the IME and paste writers deliver the
/// whole dictation at once, which TUIs like Claude Code's Ink UI drop or
/// garble. `ime_in_terminals` opts terminals into the configured chain.
pub fn chain_for_target(
    preferred: &str,
    category: AppCategory,
    ime_in_terminals: bool,
) -> &'static [WriterKind] {
    if category == AppCategory::Terminal && !ime_in_terminals {
        return &DEFAULT_CHAIN[2..];
    }
    chain_for(preferred)
}

/// Why a writer attempt did not succeed.
#[derive(Debug)]
pub enum WriteError {
    /// Nothing was written; the next writer can safely be tried.
    Unavailable(String),
    /// The writer may have written some or all of the text; stop the chain.
    Failed(anyhow::Error),
}

/// A writer that was skipped, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fallthrough {
    pub writer: WriterKind,
    pub reason: String,
}

/// Which writer delivered the text, and which ones fell through first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteReport {
    pub writer: WriterKind,
    pub fell_through: Vec<Fallthrough>,
}

impl WriteReport {
    /// One-line summary for logs and `diagnose`.
    pub fn describe(&self) -> String {
        if self.fell_through.is_empty() {
            self.writer.to_string()
        } else {
            let skipped: Vec<String> =
                self.fell_through.iter().map(|f| format!("{}: {}", f.writer, f.reason)).collect();
            format!("{} (fell through: {})", self.writer, skipped.join("; "))
        }
    }
}

/// One writer attempt. Implemented by the live writers and by test fakes.
pub(crate) trait Attempt {
    async fn attempt(&mut self, writer: WriterKind) -> Result<(), WriteError>;
}

/// Try each writer in `chain` until one writes the text.
pub(crate) async fn run_chain<A: Attempt>(
    chain: &[WriterKind],
    attempt: &mut A,
) -> Result<WriteReport> {
    let mut fell_through = Vec::new();
    for &writer in chain {
        match attempt.attempt(writer).await {
            Ok(()) => return Ok(WriteReport { writer, fell_through }),
            Err(WriteError::Unavailable(reason)) => {
                info!("Text writer '{}' unavailable: {}", writer, reason);
                fell_through.push(Fallthrough { writer, reason });
            }
            Err(WriteError::Failed(e)) => {
                return Err(e.context(format!("text writer '{}' failed", writer)));
            }
        }
    }
    let reasons: Vec<String> =
        fell_through.iter().map(|f| format!("{}: {}", f.writer, f.reason)).collect();
    anyhow::bail!("no text writer could write the text ({})", reasons.join("; "))
}

/// Everything the writers need to know about one injection.
pub struct WriteRequest<'a> {
    pub text: &'a str,
    /// Window class captured at recording start.
    pub window_class: Option<&'a str>,
    pub category: AppCategory,
    /// Per-word delay for the wtype tier (terminal profiles).
    pub word_delay_ms: u64,
    /// Leave the result on the clipboard (the old "backup" behaviour)
    /// instead of restoring the user's clipboard.
    pub keep_in_clipboard: bool,
    /// How long the paste writer waits after Ctrl+V before restoring the
    /// clipboard snapshot.
    pub paste_restore_delay: Duration,
}

struct LiveWriters<'a, P: FnMut(usize, usize)> {
    req: &'a WriteRequest<'a>,
    keyboard: &'a KeyboardInjector,
    on_progress: P,
    total_words: usize,
}

impl<P: FnMut(usize, usize)> Attempt for LiveWriters<'_, P> {
    async fn attempt(&mut self, writer: WriterKind) -> Result<(), WriteError> {
        match writer {
            WriterKind::Ime => {
                let text = self.req.text.to_string();
                tokio::task::spawn_blocking(move || ime::commit(&text, IME_ACTIVATE_TIMEOUT))
                    .await
                    .map_err(|e| WriteError::Failed(anyhow::anyhow!("IME task panicked: {e}")))??;
                (self.on_progress)(self.total_words, self.total_words);
                Ok(())
            }
            WriterKind::Paste => {
                let terminal = self.req.category == AppCategory::Terminal;
                let mut clipboard = paste::SystemClipboard;
                paste::paste(
                    &mut clipboard,
                    self.req.text,
                    terminal,
                    !self.req.keep_in_clipboard,
                    self.req.paste_restore_delay,
                )
                .await?;
                (self.on_progress)(self.total_words, self.total_words);
                Ok(())
            }
            WriterKind::Wtype => {
                let estimate_secs = (self.req.text.len() as u64 * self.req.word_delay_ms) / 1000;
                if estimate_secs > 15 {
                    warn!(
                        "Typing will take ~{}s ({} chars at {}ms/char){}",
                        estimate_secs,
                        self.req.text.len(),
                        self.req.word_delay_ms,
                        if self.req.keep_in_clipboard {
                            " — text is already in clipboard if interrupted"
                        } else {
                            ""
                        }
                    );
                }
                self.keyboard
                    .type_text_with_progress(
                        self.req.text,
                        self.req.word_delay_ms,
                        &mut self.on_progress,
                    )
                    .await
                    .map_err(WriteError::Failed)
            }
        }
    }
}

/// Write `req.text` into the focused window through `chain`.
///
/// `on_progress(done, total)` (word counts) is called with `(0, total)`
/// first; atomic writers (IME, paste) then report `(total, total)` in one
/// step, wtype reports per word.
pub async fn write(
    chain: &[WriterKind],
    req: &WriteRequest<'_>,
    keyboard: &KeyboardInjector,
    mut on_progress: impl FnMut(usize, usize),
) -> Result<WriteReport> {
    let total_words = req.text.split_whitespace().count();
    on_progress(0, total_words);

    if req.keep_in_clipboard {
        // Opt-in backup: the result stays on the clipboard, so it survives an
        // interrupted wtype run. The paste tier skips its snapshot/restore.
        if let Err(e) = paste::wl_copy(None, req.text.as_bytes()).await {
            warn!("Failed to copy result to clipboard: {}", e);
        }
    }

    let mut live = LiveWriters { req, keyboard, on_progress, total_words };
    let report = run_chain(chain, &mut live).await?;
    info!(
        "Text written via {} ({} chars, class={})",
        report.describe(),
        req.text.chars().count(),
        req.window_class.unwrap_or("?")
    );
    if let Err(e) = save_last_write(&LastWrite::new(&report, req)) {
        warn!("Failed to record last text writer: {}", e);
    }
    Ok(report)
}

/// The most recent injection, persisted for `voice-dictation diagnose`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastWrite {
    pub writer: WriterKind,
    pub fell_through: Vec<Fallthrough>,
    pub window_class: Option<String>,
    pub chars: usize,
    pub at: chrono::DateTime<chrono::Utc>,
}

impl LastWrite {
    fn new(report: &WriteReport, req: &WriteRequest<'_>) -> Self {
        Self {
            writer: report.writer,
            fell_through: report.fell_through.clone(),
            window_class: req.window_class.map(str::to_string),
            chars: req.text.chars().count(),
            at: chrono::Utc::now(),
        }
    }

    /// `at` in local time, for display.
    pub fn local_time(&self) -> String {
        self.at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string()
    }
}

/// `$XDG_RUNTIME_DIR/voice-dictation/last-write.json`.
pub fn last_write_path() -> Result<PathBuf> {
    let dirs = xdg_paths::BaseDirs::from_env()
        .map_err(|e| anyhow::anyhow!("cannot resolve XDG runtime dir: {e}"))?;
    Ok(dirs.runtime_path("voice-dictation").join("last-write.json"))
}

fn save_last_write(record: &LastWrite) -> Result<()> {
    write_last_write_to(&last_write_path()?, record)
}

fn write_last_write_to(path: &std::path::Path, record: &LastWrite) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(record)?)?;
    Ok(())
}

/// The last recorded injection, if the daemon has written one this session.
pub fn load_last_write() -> Result<Option<LastWrite>> {
    read_last_write_from(&last_write_path()?)
}

fn read_last_write_from(path: &std::path::Path) -> Result<Option<LastWrite>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Availability of each writer, for `voice-dictation diagnose`.
pub struct WriterStatus {
    pub writer: WriterKind,
    pub detail: String,
}

/// Probe what each writer would need, without writing anything.
pub fn probe_writers() -> Vec<WriterStatus> {
    let have = |cmd: &str| {
        std::process::Command::new("which")
            .arg(cmd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    let found = |cmd: &str| format!("{cmd} {}", if have(cmd) { "found" } else { "MISSING" });

    let restore = match clipboard_owner::data_control_available() {
        Ok(true) => "full-snapshot restore via wlr-data-control".to_string(),
        Ok(false) => "no wlr-data-control: restores the best single MIME type".to_string(),
        Err(e) => format!("data-control check failed: {e}"),
    };

    vec![
        WriterStatus { writer: WriterKind::Ime, detail: ime::probe_slot().to_string() },
        WriterStatus {
            writer: WriterKind::Paste,
            detail: format!(
                "{}, {}, {} (keystroke); {}",
                found("wl-copy"),
                found("wl-paste"),
                found("wtype"),
                restore
            ),
        },
        WriterStatus { writer: WriterKind::Wtype, detail: found("wtype") },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    type Outcome = fn() -> Result<(), WriteError>;

    /// Fake writers: each kind returns a scripted result; calls are recorded.
    struct Scripted {
        results: HashMap<WriterKind, Outcome>,
        calls: Vec<WriterKind>,
    }

    impl Scripted {
        fn new(results: &[(WriterKind, Outcome)]) -> Self {
            Self { results: results.iter().copied().collect(), calls: Vec::new() }
        }
    }

    impl Attempt for Scripted {
        async fn attempt(&mut self, writer: WriterKind) -> Result<(), WriteError> {
            self.calls.push(writer);
            (self.results[&writer])()
        }
    }

    fn ok() -> Result<(), WriteError> {
        Ok(())
    }
    fn unavailable() -> Result<(), WriteError> {
        Err(WriteError::Unavailable("nope".into()))
    }
    fn failed() -> Result<(), WriteError> {
        Err(WriteError::Failed(anyhow::anyhow!("boom")))
    }

    #[tokio::test]
    async fn first_writer_wins() {
        let mut s = Scripted::new(&[
            (WriterKind::Ime, ok),
            (WriterKind::Paste, ok),
            (WriterKind::Wtype, ok),
        ]);
        let r = run_chain(&DEFAULT_CHAIN, &mut s).await.unwrap();
        assert_eq!(r.writer, WriterKind::Ime);
        assert!(r.fell_through.is_empty());
        assert_eq!(s.calls, vec![WriterKind::Ime]);
    }

    #[tokio::test]
    async fn unavailable_falls_through_in_order() {
        let mut s = Scripted::new(&[
            (WriterKind::Ime, unavailable),
            (WriterKind::Paste, unavailable),
            (WriterKind::Wtype, ok),
        ]);
        let r = run_chain(&DEFAULT_CHAIN, &mut s).await.unwrap();
        assert_eq!(r.writer, WriterKind::Wtype);
        assert_eq!(
            r.fell_through.iter().map(|f| f.writer).collect::<Vec<_>>(),
            vec![WriterKind::Ime, WriterKind::Paste]
        );
        assert_eq!(r.describe(), "wtype (fell through: ime: nope; paste: nope)");
    }

    #[tokio::test]
    async fn failure_stops_the_chain() {
        // A writer that may have written must not be followed by another one.
        let mut s = Scripted::new(&[
            (WriterKind::Ime, failed),
            (WriterKind::Paste, ok),
            (WriterKind::Wtype, ok),
        ]);
        let err = run_chain(&DEFAULT_CHAIN, &mut s).await.unwrap_err();
        assert!(format!("{err:#}").contains("text writer 'ime' failed"));
        assert_eq!(s.calls, vec![WriterKind::Ime]);
    }

    #[tokio::test]
    async fn all_unavailable_is_an_error() {
        let mut s = Scripted::new(&[
            (WriterKind::Ime, unavailable),
            (WriterKind::Paste, unavailable),
            (WriterKind::Wtype, unavailable),
        ]);
        let err = run_chain(&DEFAULT_CHAIN, &mut s).await.unwrap_err();
        assert!(err.to_string().contains("no text writer could write"));
        assert_eq!(s.calls.len(), 3);
    }

    #[test]
    fn chain_starts_at_preferred_writer() {
        assert_eq!(chain_for("ime"), &DEFAULT_CHAIN);
        assert_eq!(chain_for("auto"), &DEFAULT_CHAIN);
        assert_eq!(chain_for(" Paste "), &[WriterKind::Paste, WriterKind::Wtype]);
        assert_eq!(chain_for("wtype"), &[WriterKind::Wtype]);
        assert_eq!(chain_for("bogus"), &DEFAULT_CHAIN);
    }

    #[test]
    fn terminals_use_wtype_unless_opted_in() {
        let wtype_only: &[WriterKind] = &[WriterKind::Wtype];
        assert_eq!(chain_for_target("ime", AppCategory::Terminal, false), wtype_only);
        assert_eq!(chain_for_target("paste", AppCategory::Terminal, false), wtype_only);
        assert_eq!(chain_for_target("ime", AppCategory::Terminal, true), &DEFAULT_CHAIN);
        assert_eq!(
            chain_for_target("paste", AppCategory::Terminal, true),
            &[WriterKind::Paste, WriterKind::Wtype]
        );
        assert_eq!(chain_for_target("ime", AppCategory::General, false), &DEFAULT_CHAIN);
        assert_eq!(chain_for_target("ime", AppCategory::Browser, false), &DEFAULT_CHAIN);
    }

    #[test]
    fn last_write_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/last-write.json");
        assert_eq!(read_last_write_from(&path).unwrap(), None);

        let record = LastWrite {
            writer: WriterKind::Paste,
            fell_through: vec![Fallthrough {
                writer: WriterKind::Ime,
                reason: "another input method owns the slot".into(),
            }],
            window_class: Some("firefox".into()),
            chars: 12,
            at: chrono::Utc::now(),
        };
        write_last_write_to(&path, &record).unwrap();
        assert_eq!(read_last_write_from(&path).unwrap(), Some(record));

        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["writer"], "paste");
        assert_eq!(v["fell_through"][0]["writer"], "ime");
    }
}
