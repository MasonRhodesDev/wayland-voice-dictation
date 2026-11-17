use std::sync::{Arc, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

const CONNECTED_THRESHOLD: Duration = Duration::from_secs(5);

const RETRY_INTERVAL: Duration = Duration::from_secs(2);

const MAX_CONSECUTIVE_FAILURES: u32 = 10;

const CIRCUIT_BREAKER_TIMEOUT: Duration = Duration::from_secs(60);

static ACTIVE_MONITOR: OnceLock<Arc<RwLock<String>>> = OnceLock::new();

static COMPOSITOR_CHANGED_AT: OnceLock<Arc<RwLock<Option<Instant>>>> = OnceLock::new();

fn changed_at_cell() -> &'static Arc<RwLock<Option<Instant>>> {
    COMPOSITOR_CHANGED_AT.get_or_init(|| Arc::new(RwLock::new(None)))
}

pub fn note_compositor_change(reason: &str) {
    info!("Compositor change ({reason}); will verify surfaces after events settle");
    if let Ok(mut guard) = changed_at_cell().write() {
        *guard = Some(Instant::now());
    }
    if let Some(name) = get_active_monitor_sync() {
        set_active_monitor(name);
    }
}

pub fn pending_compositor_change() -> Option<Instant> {
    changed_at_cell().read().ok().and_then(|guard| *guard)
}

pub fn clear_compositor_change() {
    if let Ok(mut guard) = changed_at_cell().write() {
        *guard = None;
    }
}

fn set_active_monitor(name: String) {
    if let Some(cell) = ACTIVE_MONITOR.get() {
        if let Ok(mut guard) = cell.write() {
            *guard = name;
        }
    }
}

pub fn get_active_monitor() -> Option<String> {
    ACTIVE_MONITOR.get().and_then(|m| m.read().ok().map(|s| s.clone()))
}

pub fn get_active_monitor_sync() -> Option<String> {
    let value = hypr_ipc::hyprctl_json_std(&["-j", "monitors"]).ok()?;
    value.as_array()?.iter().find_map(|monitor| {
        if monitor.get("focused").and_then(|v| v.as_bool()).unwrap_or(false) {
            monitor.get("name").and_then(|n| n.as_str()).map(str::to_string)
        } else {
            None
        }
    })
}

const FALLBACK_MONITOR: &str = "FALLBACK";

fn monitor_names_in(value: &serde_json::Value) -> Option<Vec<String>> {
    Some(
        value
            .as_array()?
            .iter()
            .filter_map(|monitor| monitor.get("name").and_then(|n| n.as_str()))
            .filter(|name| *name != FALLBACK_MONITOR)
            .map(str::to_string)
            .collect(),
    )
}

pub fn get_monitor_names_sync() -> Option<Vec<String>> {
    let value = hypr_ipc::hyprctl_json_std(&["-j", "monitors"]).ok()?;
    monitor_names_in(&value)
}

fn refresh_hyprland_environment() -> bool {
    match hypr_ipc::socket2_path() {
        Ok(path) => {
            debug!("Hyprland socket verified: {}", path.display());
            true
        }
        Err(_) => {
            debug!("Hyprland event socket not found");
            false
        }
    }
}

pub fn spawn_active_monitor_listener(wake: Arc<dyn Fn() + Send + Sync>) {
    let initial = get_active_monitor_sync();
    info!("Initial active monitor from Hyprland IPC: {:?}", initial);
    let monitor = Arc::new(RwLock::new(initial.unwrap_or_default()));
    let _ = ACTIVE_MONITOR.set(monitor.clone());
    let _ = changed_at_cell();

    thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
            warn!("failed to start Hyprland monitor listener runtime");
            return;
        };
        rt.block_on(listen_socket2(monitor, wake));
    });
}

async fn listen_socket2(monitor: Arc<RwLock<String>>, wake: Arc<dyn Fn() + Send + Sync>) {
    use tokio::io::AsyncBufReadExt;

    let mut consecutive_failures: u32 = 0;
    loop {
        refresh_hyprland_environment();
        let started = Instant::now();
        match hypr_ipc::connect_socket2().await {
            Ok(stream) => {
                info!("connected to Hyprland event socket");
                consecutive_failures = 0;
                let mut lines = tokio::io::BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Some(frame) = hypr_ipc::parse_line(&line) else {
                        continue;
                    };
                    match frame.event.as_str() {
                        "focusedmon" => {
                            let name = frame.payload.split(',').next().unwrap_or("").to_string();
                            if let Ok(mut current) = monitor.write() {
                                let old_monitor = current.clone();
                                debug!(
                                    "Active monitor changed from '{}' to '{}'",
                                    old_monitor, name
                                );
                                *current = name.clone();
                                if old_monitor != name {
                                    wake();
                                }
                            }
                        }
                        "monitoraddedv2" => {
                            let name = frame.payload.split(',').nth(1).unwrap_or("");
                            note_compositor_change(&format!("monitor added: '{name}'"));
                            wake();
                        }
                        "monitorremoved" => {
                            note_compositor_change(&format!(
                                "monitor removed: '{}'",
                                frame.payload
                            ));
                            wake();
                        }
                        "configreloaded" => {
                            note_compositor_change("hyprland config reloaded");
                            wake();
                        }
                        _ => {}
                    }
                }
                let was_connected = started.elapsed() >= CONNECTED_THRESHOLD;
                if was_connected {
                    warn!("Hyprland IPC connection lost after {}s", started.elapsed().as_secs());
                    set_active_monitor(String::new());
                    note_compositor_change("hyprland IPC connection lost");
                    wake();
                } else {
                    warn!("Hyprland event socket closed before staying connected");
                }
            }
            Err(e) => {
                consecutive_failures += 1;
                warn!(
                    "Hyprland event listener connection failed (attempt {}/{}): {e}",
                    consecutive_failures, MAX_CONSECUTIVE_FAILURES
                );
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    warn!(
                        "Hyprland monitor listener failed {} times, backing off for {}s",
                        consecutive_failures,
                        CIRCUIT_BREAKER_TIMEOUT.as_secs()
                    );
                    tokio::time::sleep(CIRCUIT_BREAKER_TIMEOUT).await;
                    consecutive_failures = 0;
                }
            }
        }
        tokio::time::sleep(RETRY_INTERVAL).await;
        if let Some(name) = get_active_monitor_sync() {
            set_active_monitor(name);
            wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::monitor_names_in;

    #[test]
    fn real_monitors_are_listed() {
        let value = serde_json::json!([{ "name": "DP-1" }, { "name": "eDP-2" }]);
        assert_eq!(monitor_names_in(&value), Some(vec!["DP-1".to_string(), "eDP-2".to_string()]));
    }

    #[test]
    fn the_fallback_placeholder_is_not_a_monitor() {
        let value = serde_json::json!([{ "name": "FALLBACK" }]);
        assert_eq!(monitor_names_in(&value), Some(Vec::new()));
    }
}
