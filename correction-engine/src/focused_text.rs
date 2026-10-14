//! One-shot read of the focused text field over AT-SPI2.
//!
//! Used by context biasing to see the text around the cursor in the window
//! that dictation will type into. Unlike [`crate::AtspiConnection`], which
//! listens for change events, this asks the target application directly:
//!
//! 1. Find the AT-SPI application whose D-Bus connection belongs to the
//!    target window's process. Only that application is ever read.
//! 2. Find its focused object, via `Collection.GetMatches` when the toolkit
//!    implements it, else a bounded walk of the active window.
//! 3. Read a window of text around the caret. Password fields are skipped.
//!
//! Every failure (bus off, app without accessibility, no focused text) is an
//! `Ok(None)` or an error the caller can ignore; nothing here is required.

use anyhow::Result;
use atspi::proxy::accessible::{AccessibleProxy, ObjectRefExt};
use atspi::proxy::proxy_ext::ProxyExt;
use atspi::{
    AccessibilityConnection, MatchType, ObjectMatchRule, ObjectRefOwned, Role, SortOrder, State,
};
use tracing::debug;

/// Most objects visited by the fallback walk before giving up.
const MAX_WALK_NODES: usize = 400;
/// Deepest level visited by the fallback walk.
const MAX_WALK_DEPTH: usize = 40;

/// Reads the focused text field of a given process over the AT-SPI2 bus.
pub struct FocusedTextReader {
    conn: AccessibilityConnection,
}

impl FocusedTextReader {
    /// Connect to the accessibility bus. Fails when the bus is not running.
    pub async fn connect() -> Result<Self> {
        Ok(Self { conn: AccessibilityConnection::new().await? })
    }

    /// Text around the caret in the focused field of the application owned
    /// by `pid`, at most `max_chars` characters. `None` when that application
    /// is not on the bus, nothing is focused, the focused object has no text,
    /// or it is a password field.
    pub async fn read(&self, pid: u32, max_chars: usize) -> Result<Option<String>> {
        let conn = self.conn.connection();
        let Some(app) = self.app_for_pid(pid).await? else {
            debug!("focused_text: no accessible application for pid {}", pid);
            return Ok(None);
        };
        let app = app.as_accessible_proxy(conn).await?;
        let Some(focused) = find_focused(conn, &app).await? else {
            debug!("focused_text: no focused object for pid {}", pid);
            return Ok(None);
        };
        let focused = focused.as_accessible_proxy(conn).await?;
        if focused.get_role().await? == Role::PasswordText {
            return Ok(None);
        }
        let Ok(text) = focused.proxies().await?.text().await else {
            return Ok(None);
        };
        let count = text.character_count().await?;
        let caret = text.caret_offset().await.unwrap_or(-1);
        let (start, end) = caret_window(count, caret, max_chars);
        if end <= start {
            return Ok(None);
        }
        let content = text.get_text(start, end).await?;
        Ok(Some(content).filter(|s| !s.trim().is_empty()))
    }

    /// The application whose bus connection belongs to `pid`.
    async fn app_for_pid(&self, pid: u32) -> Result<Option<ObjectRefOwned>> {
        let conn = self.conn.connection();
        let root = self.conn.root_accessible_on_registry().await?;
        let dbus = zbus::fdo::DBusProxy::new(conn).await?;
        for app in root.get_children().await? {
            let Some(name) = app.name() else { continue };
            let owner = dbus.get_connection_unix_process_id(name.clone().into()).await;
            if owner.ok() == Some(pid) {
                return Ok(Some(app));
            }
        }
        Ok(None)
    }
}

/// The focused descendant of `app`.
async fn find_focused(
    conn: &zbus::Connection,
    app: &AccessibleProxy<'_>,
) -> Result<Option<ObjectRefOwned>> {
    if let Ok(collection) = app.proxies().await?.collection().await {
        let rule = ObjectMatchRule::builder().states([State::Focused], MatchType::All).build();
        match collection.get_matches(rule, SortOrder::Canonical, 1, true).await {
            Ok(found) => {
                if let Some(obj) = found.into_iter().next() {
                    return Ok(Some(obj));
                }
            }
            Err(e) => debug!("focused_text: Collection.GetMatches failed: {}", e),
        }
    }
    walk_for_focused(conn, app).await
}

/// Depth-first search of the active window(s) for a focused object, visiting
/// only showing objects and at most [`MAX_WALK_NODES`] of them.
async fn walk_for_focused(
    conn: &zbus::Connection,
    app: &AccessibleProxy<'_>,
) -> Result<Option<ObjectRefOwned>> {
    let windows = app.get_children().await?;
    let mut active = Vec::new();
    for window in &windows {
        let proxy = window.as_accessible_proxy(conn).await?;
        if proxy.get_state().await.is_ok_and(|s| s.contains(State::Active)) {
            active.push(window.clone());
        }
    }
    let mut stack: Vec<(ObjectRefOwned, usize)> =
        if active.is_empty() { windows } else { active }.into_iter().map(|w| (w, 0)).collect();

    let mut visited = 0;
    while let Some((node, depth)) = stack.pop() {
        visited += 1;
        if visited > MAX_WALK_NODES {
            debug!("focused_text: walk budget exhausted");
            break;
        }
        let proxy = node.as_accessible_proxy(conn).await?;
        let Ok(state) = proxy.get_state().await else { continue };
        if state.contains(State::Focused) {
            return Ok(Some(node.clone()));
        }
        if depth >= MAX_WALK_DEPTH || (depth > 0 && !state.contains(State::Showing)) {
            continue;
        }
        if let Ok(children) = proxy.get_children().await {
            stack.extend(children.into_iter().rev().map(|c| (c, depth + 1)));
        }
    }
    Ok(None)
}

/// The `[start, end)` character range to read: the whole text when it fits,
/// else `max_chars` centred on the caret (or the end, without a caret).
fn caret_window(count: i32, caret: i32, max_chars: usize) -> (i32, i32) {
    let max = i32::try_from(max_chars).unwrap_or(i32::MAX);
    if count <= 0 || max <= 0 {
        return (0, 0);
    }
    if count <= max {
        return (0, count);
    }
    let caret = if caret < 0 { count } else { caret.min(count) };
    let start = (caret - max / 2).clamp(0, count - max);
    (start, start + max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_read_whole() {
        assert_eq!(caret_window(50, 10, 100), (0, 50));
    }

    #[test]
    fn long_text_centres_on_caret() {
        assert_eq!(caret_window(1000, 500, 100), (450, 550));
    }

    #[test]
    fn window_is_clamped_to_the_text() {
        assert_eq!(caret_window(1000, 10, 100), (0, 100));
        assert_eq!(caret_window(1000, 990, 100), (900, 1000));
    }

    #[test]
    fn missing_caret_reads_the_end() {
        assert_eq!(caret_window(1000, -1, 100), (900, 1000));
    }

    /// Manual check on a live desktop: focus a text field, then run
    /// `FOCUSED_TEXT_PID=<pid of that window> cargo test -p correction-engine
    /// live_focused_text -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "needs a live accessibility bus and a focused text field"]
    async fn live_focused_text() {
        let pid: u32 =
            std::env::var("FOCUSED_TEXT_PID").expect("set FOCUSED_TEXT_PID").parse().unwrap();
        let reader = FocusedTextReader::connect().await.expect("accessibility bus");
        let text = reader.read(pid, 2000).await.expect("read");
        println!("focused text for pid {pid}: {text:?}");
    }

    #[test]
    fn empty_or_zero_budget_reads_nothing() {
        assert_eq!(caret_window(0, 0, 100), (0, 0));
        assert_eq!(caret_window(-1, 0, 100), (0, 0));
        assert_eq!(caret_window(100, 0, 0), (0, 0));
    }
}
