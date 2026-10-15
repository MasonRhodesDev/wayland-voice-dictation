//! Where context biasing gets its text: one [`ContextProvider`] per source.
//!
//! At recording start every provider is asked, concurrently and under one
//! shared [`PROVIDER_TIMEOUT`], for text from the window dictation will type
//! into. All answers are merged in [`ContextKind`] priority order, so the
//! focused field outranks the terminal pane, which outranks the window title,
//! when [`crate::context_words`] caps the word list.
//!
//! Providers today: window title (always), and with the `correction` feature
//! the wezterm pane and the AT-SPI focused field. The input-method
//! `surrounding_text` provider, the primary source for the focused field,
//! comes with the input-method writer (#17): it reuses that connection rather
//! than binding a second input method on the seat.

use std::time::Duration;

use futures_util::future::{join_all, BoxFuture};
use tracing::debug;

use crate::context_words::{self, ContextWords};

/// Budget for all providers together. Recording start waits on it (audio is
/// already being captured), so a slow source is dropped, not waited for.
pub const PROVIDER_TIMEOUT: Duration = Duration::from_millis(250);

/// Most characters read around the caret of the focused field.
#[cfg_attr(not(feature = "correction"), allow(dead_code))]
const MAX_FOCUSED_CHARS: usize = 2000;

/// The kind of source, declared in merge priority order (highest first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContextKind {
    /// Input-method `surrounding_text` of the focused field.
    // Constructed once the input-method writer (#17) lands.
    #[allow(dead_code)]
    InputMethod,
    /// AT-SPI2 text of the focused field.
    #[cfg_attr(not(feature = "correction"), allow(dead_code))]
    Accessibility,
    /// Visible text of the active terminal pane.
    #[cfg_attr(not(feature = "correction"), allow(dead_code))]
    Terminal,
    /// The focused window's title.
    WindowTitle,
}

/// The window a recording targets, captured at recording start.
#[derive(Debug, Clone, Default)]
// Without the `correction` feature only the title provider exists.
#[cfg_attr(not(feature = "correction"), allow(dead_code))]
pub struct ContextTarget {
    pub class: String,
    pub title: String,
    pub pid: Option<u32>,
}

/// One source of context text.
pub trait ContextProvider: Send + Sync {
    fn kind(&self) -> ContextKind;

    /// Text from this source for `target`, or `None` when it has nothing
    /// (wrong kind of window, source unavailable, empty).
    fn collect<'a>(&'a self, target: &'a ContextTarget) -> BoxFuture<'a, Option<String>>;
}

/// Text gathered for one recording, sorted by [`ContextKind`] priority.
#[derive(Debug, Clone, Default)]
pub struct ContextSources {
    texts: Vec<(ContextKind, String)>,
}

impl ContextSources {
    pub fn new(mut texts: Vec<(ContextKind, String)>) -> Self {
        texts.retain(|(_, t)| !t.trim().is_empty());
        texts.sort_by_key(|(kind, _)| *kind);
        Self { texts }
    }

    pub fn kinds(&self) -> Vec<ContextKind> {
        self.texts.iter().map(|(k, _)| *k).collect()
    }

    /// Filter all sources down to at most `cap` distinctive words.
    pub fn extract(&self, cap: usize) -> ContextWords {
        let sources: Vec<&str> = self.texts.iter().map(|(_, t)| t.as_str()).collect();
        ContextWords::new(context_words::extract(&sources, cap))
    }
}

/// Ask every provider for `target`, concurrently, within [`PROVIDER_TIMEOUT`].
pub async fn collect(
    providers: &[Box<dyn ContextProvider>],
    target: &ContextTarget,
) -> ContextSources {
    collect_within(providers, target, PROVIDER_TIMEOUT).await
}

async fn collect_within(
    providers: &[Box<dyn ContextProvider>],
    target: &ContextTarget,
    budget: Duration,
) -> ContextSources {
    let asks = providers.iter().map(|p| async move {
        match tokio::time::timeout(budget, p.collect(target)).await {
            Ok(text) => text.map(|t| (p.kind(), t)),
            Err(_) => {
                debug!("context: {:?} provider timed out", p.kind());
                None
            }
        }
    });
    ContextSources::new(join_all(asks).await.into_iter().flatten().collect())
}

/// The providers available in this build.
pub fn default_providers() -> Vec<Box<dyn ContextProvider>> {
    #[allow(unused_mut)]
    let mut providers: Vec<Box<dyn ContextProvider>> = vec![Box::new(WindowTitleProvider)];
    #[cfg(feature = "correction")]
    {
        providers.push(Box::new(WeztermPaneProvider));
        providers.push(Box::new(AtspiFocusProvider::default()));
    }
    providers
}

/// The focused window's title (from hyprctl at recording start).
struct WindowTitleProvider;

impl ContextProvider for WindowTitleProvider {
    fn kind(&self) -> ContextKind {
        ContextKind::WindowTitle
    }

    fn collect<'a>(&'a self, target: &'a ContextTarget) -> BoxFuture<'a, Option<String>> {
        let title = Some(target.title.clone()).filter(|t| !t.trim().is_empty());
        Box::pin(async move { title })
    }
}

/// Visible text of the active pane, when the target is wezterm.
#[cfg(feature = "correction")]
struct WeztermPaneProvider;

#[cfg(feature = "correction")]
impl ContextProvider for WeztermPaneProvider {
    fn kind(&self) -> ContextKind {
        ContextKind::Terminal
    }

    fn collect<'a>(&'a self, target: &'a ContextTarget) -> BoxFuture<'a, Option<String>> {
        use correction_engine::wezterm::{WeztermCli, WeztermClient};

        Box::pin(async move {
            if target.class != correction_engine::WEZTERM_WINDOW_CLASS {
                return None;
            }
            let cli = WeztermCli::discover()?;
            let panes = cli.list_panes().await.ok()?;
            let pane = pick_pane(&panes, &target.title)?;
            cli.get_text(pane).await.ok()
        })
    }
}

/// The pane the user is looking at: the active pane whose title appears in
/// the window title (wezterm titles the window after its active pane). When
/// no pane matches, or several active panes in different windows do, the
/// focused pane is unknown and `None` is returned, rather than reading a pane
/// in some other window.
#[cfg(feature = "correction")]
fn pick_pane(panes: &[correction_engine::wezterm::PaneEntry], window_title: &str) -> Option<u64> {
    let mut matches = panes
        .iter()
        .filter(|p| p.is_active && !p.title.is_empty() && window_title.contains(&p.title));
    let first = matches.next()?;
    match matches.next() {
        None => Some(first.pane_id),
        Some(_) => None,
    }
}

/// Text around the caret of the focused field, over AT-SPI2. The fallback
/// for the input-method source: works for any toolkit with accessibility on,
/// and returns nothing when the accessibility bus or the app's tree is off.
#[cfg(feature = "correction")]
#[derive(Default)]
struct AtspiFocusProvider {
    /// Connected lazily and kept; a failed or timed-out connect is retried at
    /// the next recording, so turning accessibility on later still works.
    reader: tokio::sync::Mutex<Option<std::sync::Arc<correction_engine::FocusedTextReader>>>,
}

#[cfg(feature = "correction")]
impl ContextProvider for AtspiFocusProvider {
    fn kind(&self) -> ContextKind {
        ContextKind::Accessibility
    }

    fn collect<'a>(&'a self, target: &'a ContextTarget) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let pid = target.pid?;
            let reader = {
                let mut slot = self.reader.lock().await;
                if slot.is_none() {
                    match correction_engine::FocusedTextReader::connect().await {
                        Ok(r) => *slot = Some(std::sync::Arc::new(r)),
                        Err(e) => {
                            debug!("context: accessibility bus unavailable: {}", e);
                            return None;
                        }
                    }
                }
                slot.clone()?
            };
            match reader.read(pid, MAX_FOCUSED_CHARS).await {
                Ok(text) => text,
                Err(e) => {
                    debug!("context: focused field read failed: {}", e);
                    None
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(ContextKind, Option<&'static str>);

    impl ContextProvider for Fixed {
        fn kind(&self) -> ContextKind {
            self.0
        }
        fn collect<'a>(&'a self, _: &'a ContextTarget) -> BoxFuture<'a, Option<String>> {
            let text = self.1.map(str::to_string);
            Box::pin(async move { text })
        }
    }

    struct Hangs(ContextKind);

    impl ContextProvider for Hangs {
        fn kind(&self) -> ContextKind {
            self.0
        }
        fn collect<'a>(&'a self, _: &'a ContextTarget) -> BoxFuture<'a, Option<String>> {
            Box::pin(std::future::pending())
        }
    }

    #[test]
    fn priority_is_input_method_then_accessibility_then_terminal_then_title() {
        use ContextKind::*;
        assert!(InputMethod < Accessibility);
        assert!(Accessibility < Terminal);
        assert!(Terminal < WindowTitle);
    }

    #[test]
    fn sources_merge_in_priority_order() {
        let sources = ContextSources::new(vec![
            (ContextKind::WindowTitle, "chezmoi".into()),
            (ContextKind::Terminal, "kubectl".into()),
            (ContextKind::Accessibility, "hyprctl".into()),
        ]);
        assert_eq!(
            sources.kinds(),
            vec![ContextKind::Accessibility, ContextKind::Terminal, ContextKind::WindowTitle]
        );
        // With room for one word, the focused field wins.
        let words = sources.extract(1);
        assert_eq!(words.glossary(), vec!["hyprctl"]);
    }

    #[test]
    fn blank_texts_are_dropped() {
        let sources = ContextSources::new(vec![
            (ContextKind::Terminal, "  \n".into()),
            (ContextKind::WindowTitle, "chezmoi".into()),
        ]);
        assert_eq!(sources.kinds(), vec![ContextKind::WindowTitle]);
    }

    #[tokio::test]
    async fn slow_and_empty_providers_are_skipped() {
        let providers: Vec<Box<dyn ContextProvider>> = vec![
            Box::new(Fixed(ContextKind::WindowTitle, Some("chezmoi"))),
            Box::new(Hangs(ContextKind::Accessibility)),
            Box::new(Fixed(ContextKind::Terminal, None)),
        ];
        let target = ContextTarget::default();
        let sources = collect_within(&providers, &target, Duration::from_millis(20)).await;
        assert_eq!(sources.kinds(), vec![ContextKind::WindowTitle]);
    }

    #[tokio::test]
    async fn window_title_provider_skips_empty_titles() {
        let target = ContextTarget { title: "  ".into(), ..ContextTarget::default() };
        assert_eq!(WindowTitleProvider.collect(&target).await, None);
        let target = ContextTarget { title: "nvim".into(), ..ContextTarget::default() };
        assert_eq!(WindowTitleProvider.collect(&target).await.as_deref(), Some("nvim"));
    }

    #[cfg(feature = "correction")]
    #[tokio::test]
    async fn atspi_provider_needs_a_pid() {
        let target = ContextTarget { class: "zenity".into(), ..ContextTarget::default() };
        assert_eq!(AtspiFocusProvider::default().collect(&target).await, None);
    }

    #[cfg(feature = "correction")]
    #[tokio::test]
    async fn wezterm_provider_ignores_other_windows() {
        let target = ContextTarget { class: "firefox".into(), ..ContextTarget::default() };
        assert_eq!(WeztermPaneProvider.collect(&target).await, None);
    }

    #[cfg(feature = "correction")]
    #[test]
    fn pick_pane_prefers_title_match() {
        use correction_engine::wezterm::PaneEntry;
        let pane = |pane_id, is_active, title: &str| PaneEntry {
            pane_id,
            is_active,
            title: title.to_string(),
            workspace: String::new(),
        };
        let panes = vec![pane(1, false, "nvim"), pane(2, true, "zsh"), pane(3, true, "nvim")];
        assert_eq!(pick_pane(&panes, "[1/2] nvim"), Some(3));
        // No active pane matches the focused window: no pane context, not
        // the first active pane of some other window.
        assert_eq!(pick_pane(&panes, "something else"), None);
        assert_eq!(pick_pane(&[pane(1, false, "x")], "x"), None);
        // Two windows whose active panes both match: ambiguous, so none.
        let twins = vec![pane(4, true, "zsh"), pane(5, true, "zsh")];
        assert_eq!(pick_pane(&twins, "zsh"), None);
    }
}
