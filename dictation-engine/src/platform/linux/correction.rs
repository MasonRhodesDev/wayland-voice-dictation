use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use correction_engine::{
    CorrectionMonitor, CorrectionStore, InjectionContext, MonitorConfig, WeztermMonitor,
    WEZTERM_WINDOW_CLASS,
};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

use crate::platform::CorrectionSettings;

pub struct CorrectionLearning {
    atspi: Option<CorrectionMonitor>,
    wezterm: Option<WeztermMonitor>,
    last_injection: Option<InjectionContext>,
    monitor_duration_secs: u64,
}

impl CorrectionLearning {
    pub async fn start(settings: &CorrectionSettings) -> Self {
        if settings.enabled && settings.accessibility_bridge {
            enable_accessibility_bridge().await;
        }

        let vd_dir = xdg_paths::ConfigDirs::from_env()
            .map(|d| d.data_dir("voice-dictation"))
            .unwrap_or_default();
        let monitor_config = || MonitorConfig {
            enabled: true,
            monitor_duration_secs: settings.monitor_duration_secs,
            auto_promote_threshold: settings.auto_promote_threshold,
            max_age_days: settings.max_age_days,
            store_path: vd_dir.join("corrections.json"),
            substitutions_path: vd_dir.join("substitutions.txt"),
        };

        let atspi = if settings.enabled {
            match CorrectionMonitor::new(monitor_config()).await {
                Ok(monitor) => {
                    if monitor.is_available() {
                        info!("Correction learning enabled (AT-SPI2 available)");
                    } else {
                        warn!("Correction learning enabled but AT-SPI2 bus unavailable; corrections will not be detected");
                    }
                    Some(monitor)
                }
                Err(e) => {
                    warn!("Failed to initialize correction monitor: {}; continuing without correction learning", e);
                    None
                }
            }
        } else {
            info!("Correction learning disabled");
            None
        };

        let wezterm = if settings.enabled && settings.wezterm_backend {
            let monitor = match atspi.as_ref() {
                Some(atspi_monitor) => Some(WeztermMonitor::with_shared_store(
                    atspi_monitor.store_handle(),
                    monitor_config(),
                )),
                None => match WeztermMonitor::new(monitor_config()) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        warn!("Failed to initialize wezterm correction backend: {}", e);
                        None
                    }
                },
            };
            if monitor.is_some() {
                info!(
                    "wezterm correction backend enabled (live socket now: {})",
                    WeztermMonitor::is_available()
                );
            }
            monitor
        } else {
            None
        };

        let store_handle = atspi
            .as_ref()
            .map(|m| m.store_handle())
            .or_else(|| wezterm.as_ref().map(|m| m.store_handle()));
        if let Some(store) = store_handle {
            let corrections_path = vd_dir.join("corrections.json");
            tokio::spawn(async move {
                if let Err(e) = watch_corrections_file(store, corrections_path).await {
                    error!("Corrections file watcher error: {}", e);
                }
            });
        }

        Self {
            atspi,
            wezterm,
            last_injection: None,
            monitor_duration_secs: settings.monitor_duration_secs,
        }
    }

    pub fn record_injection(&mut self, text: &str, window_class: &str) {
        let context = InjectionContext {
            text: text.to_string(),
            timestamp: chrono::Utc::now(),
            instant: Instant::now(),
            window_class: window_class.to_string(),
            window_title: String::new(),
        };
        self.last_injection = Some(context.clone());
        if let Some(backend) = self.monitor(context) {
            debug!("{} started for {}s", backend, self.monitor_duration_secs);
        }
    }

    pub fn snapshot(&self) {
        let Some(last) = self.last_injection.clone() else {
            warn!("SnapshotCorrection requested but nothing has been dictated yet");
            return;
        };
        let context =
            InjectionContext { timestamp: chrono::Utc::now(), instant: Instant::now(), ..last };
        match self.monitor(context) {
            Some(backend) => info!("Manual snapshot: re-armed {} for last injection", backend),
            None => warn!("SnapshotCorrection requested but correction learning is disabled"),
        }
    }

    fn monitor(&self, context: InjectionContext) -> Option<&'static str> {
        let is_wezterm = context.window_class == WEZTERM_WINDOW_CLASS;
        match (is_wezterm, &self.wezterm, &self.atspi) {
            (true, Some(monitor), _) => {
                let _handle = monitor.start_monitoring(context);
                Some("wezterm correction monitoring")
            }
            (_, _, Some(monitor)) => {
                let _handle = monitor.start_monitoring(context);
                Some("correction monitoring")
            }
            _ => None,
        }
    }
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

async fn watch_corrections_file(store: Arc<Mutex<CorrectionStore>>, path: PathBuf) -> Result<()> {
    info!("Watching corrections file: {:?}", path);

    let (tx, mut rx) = mpsc::channel(100);

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res {
            let _ = tx.blocking_send(event);
        }
    })?;

    if path.exists() {
        watcher.watch(&path, RecursiveMode::NonRecursive)?;
    } else if let Some(parent) = path.parent() {
        if parent.exists() {
            watcher.watch(parent, RecursiveMode::NonRecursive)?;
        }
    }

    while let Some(event) = rx.recv().await {
        match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                if event.paths.iter().any(|p| p == &path) =>
            {
                debug!("Corrections file changed: {:?}, reloading...", path);
                if let Err(e) = store.lock().await.reload() {
                    warn!("Failed to reload corrections store: {}", e);
                } else {
                    debug!("Corrections store reloaded from disk");
                }
            }
            _ => {}
        }
    }

    Ok(())
}
