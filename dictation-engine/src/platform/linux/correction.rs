use std::sync::Arc;

use correction_engine::{AtspiConnection, TextChangeSource, WEZTERM_WINDOW_CLASS};
use tracing::{debug, info, warn};

use crate::platform::CorrectionSettings;

pub(crate) const WEZTERM_APP_ID: &str = WEZTERM_WINDOW_CLASS;

pub(crate) async fn text_change_source(settings: &CorrectionSettings) -> Arc<dyn TextChangeSource> {
    if settings.accessibility_bridge {
        enable_accessibility_bridge().await;
    }
    Arc::new(AtspiConnection::connect().await)
}

async fn enable_accessibility_bridge() {
    match tokio::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "toolkit-accessibility"])
        .output()
        .await
    {
        Ok(output) => {
            let current = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if current == "false" {
                info!("Enabling accessibility bridge (toolkit-accessibility → true)");
                let _ = tokio::process::Command::new("gsettings")
                    .args(["set", "org.gnome.desktop.interface", "toolkit-accessibility", "true"])
                    .output()
                    .await;
            } else {
                debug!("Accessibility bridge already enabled");
            }
        }
        Err(e) => {
            warn!("Could not check accessibility bridge via gsettings: {}; correction detection may not work for GTK apps", e);
        }
    }
}
