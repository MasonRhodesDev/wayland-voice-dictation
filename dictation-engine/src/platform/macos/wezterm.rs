use std::path::PathBuf;

use objc2::rc::autoreleasepool;
use objc2_app_kit::NSWorkspace;
use objc2_foundation::NSString;

use crate::platform::WeztermInstall;

pub(crate) const WEZTERM_APP_ID: &str = "com.github.wez.wezterm";

pub fn wezterm_install() -> Option<WeztermInstall> {
    let app = autoreleasepool(|_| {
        let id = NSString::from_str(WEZTERM_APP_ID);
        let url = NSWorkspace::sharedWorkspace().URLForApplicationWithBundleIdentifier(&id)?;
        Some(PathBuf::from(url.path()?.to_string()))
    })?;
    Some(WeztermInstall {
        program: app.join("Contents/MacOS/wezterm"),
        socket_dir: dirs::home_dir()?.join(".local/share/wezterm"),
    })
}
