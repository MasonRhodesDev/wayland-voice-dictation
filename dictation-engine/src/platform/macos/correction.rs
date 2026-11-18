use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::string::{CFString, CFStringRef};
use correction_engine::{InjectionContext, TextChangeEvent, TextChangeSource};
use objc2::rc::autoreleasepool;
use objc2_app_kit::NSRunningApplication;
use tokio::sync::mpsc;
use tracing::debug;

use super::permissions::accessibility_granted;
use crate::platform::text_edits::EditTracker;
use crate::platform::CorrectionSettings;

pub(crate) const WEZTERM_APP_ID: &str = "com.github.wez.wezterm";

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const LOCATE_TIMEOUT: Duration = Duration::from_secs(3);
const SECURE_TEXT_FIELD: &str = "AXSecureTextField";
const AX_SUCCESS: i32 = 0;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateSystemWide() -> CFTypeRef;
    fn AXUIElementCopyAttributeValue(
        element: CFTypeRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementGetPid(element: CFTypeRef, pid: *mut libc::pid_t) -> i32;
}

pub(crate) async fn text_change_source(
    _settings: &CorrectionSettings,
) -> Arc<dyn TextChangeSource> {
    Arc::new(AccessibilityText)
}

struct AccessibilityText;

#[async_trait]
impl TextChangeSource for AccessibilityText {
    async fn subscribe(
        &self,
        context: &InjectionContext,
    ) -> Result<mpsc::Receiver<TextChangeEvent>> {
        let (tx, rx) = mpsc::channel(256);
        let injected = context.text.clone();
        thread::Builder::new()
            .name("ax-text-watch".into())
            .spawn(move || watch_focused_field(&injected, &tx))?;
        Ok(rx)
    }

    fn is_available(&self) -> bool {
        accessibility_granted()
    }
}

fn watch_focused_field(injected: &str, tx: &mpsc::Sender<TextChangeEvent>) {
    let Some((field, mut tracker)) = locate_injection(injected, tx) else {
        debug!("Injected text not found in the focused field; skipping correction monitoring");
        return;
    };
    while tracker.is_tracking() && !tx.is_closed() {
        thread::sleep(POLL_INTERVAL);
        let Some(text) = field.value() else {
            continue;
        };
        for event in tracker.observe(&text, Instant::now()) {
            if tx.blocking_send(event).is_err() {
                return;
            }
        }
    }
}

fn locate_injection(
    injected: &str,
    tx: &mpsc::Sender<TextChangeEvent>,
) -> Option<(TextField, EditTracker)> {
    let system = Element(unsafe { CFType::wrap_under_create_rule(AXUIElementCreateSystemWide()) });
    let deadline = Instant::now() + LOCATE_TIMEOUT;
    while Instant::now() < deadline && !tx.is_closed() {
        let field = system.attribute("AXFocusedUIElement").map(Element).and_then(TextField::new);
        if let Some(field) = field {
            let tracker = field
                .value()
                .and_then(|text| EditTracker::locate(&text, injected, field.bundle_id()));
            if let Some(tracker) = tracker {
                return Some((field, tracker));
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
    None
}

struct Element(CFType);

impl Element {
    fn attribute(&self, name: &'static str) -> Option<CFType> {
        let name = CFString::from_static_string(name);
        let mut value: CFTypeRef = std::ptr::null();
        let error = unsafe {
            AXUIElementCopyAttributeValue(
                self.0.as_CFTypeRef(),
                name.as_concrete_TypeRef(),
                &mut value,
            )
        };
        (error == AX_SUCCESS && !value.is_null())
            .then(|| unsafe { CFType::wrap_under_create_rule(value) })
    }

    fn string(&self, name: &'static str) -> Option<String> {
        self.attribute(name)?.downcast::<CFString>().map(|s| s.to_string())
    }

    fn pid(&self) -> Option<libc::pid_t> {
        let mut pid = 0;
        let error = unsafe { AXUIElementGetPid(self.0.as_CFTypeRef(), &mut pid) };
        (error == AX_SUCCESS).then_some(pid)
    }
}

struct TextField(Element);

impl TextField {
    fn new(element: Element) -> Option<Self> {
        let role = element.string("AXRole")?;
        let subrole = element.string("AXSubrole");
        let secure = role == SECURE_TEXT_FIELD || subrole.as_deref() == Some(SECURE_TEXT_FIELD);
        (!secure).then_some(Self(element))
    }

    fn value(&self) -> Option<String> {
        self.0.string("AXValue")
    }

    fn bundle_id(&self) -> String {
        let Some(pid) = self.0.pid() else {
            return String::new();
        };
        autoreleasepool(|_| {
            NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
                .and_then(|app| app.bundleIdentifier())
                .map(|id| id.to_string())
                .unwrap_or_default()
        })
    }
}
