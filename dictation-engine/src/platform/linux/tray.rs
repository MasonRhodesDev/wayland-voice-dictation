use futures_util::StreamExt;
use ksni::menu::{MenuItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{Handle, Tray, TrayMethods};
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::audio_backend::BackendType;
use crate::control::{DaemonCommand, DaemonState};
use crate::platform::tray_menu::{TrayAction, TrayEntry, TrayModel};

pub struct DictationTray {
    model: TrayModel,
    command_tx: mpsc::Sender<DaemonCommand>,
    icon_invalidated: bool,
}

impl Tray for DictationTray {
    fn id(&self) -> String {
        "voice-dictation".into()
    }

    fn title(&self) -> String {
        "Voice Dictation".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn icon_name(&self) -> String {
        if self.icon_invalidated {
            return "content-loading-symbolic".into();
        }
        match self.model.state {
            DaemonState::Idle => "microphone-sensitivity-muted-symbolic",
            DaemonState::Recording => "microphone-sensitivity-high-symbolic",
            DaemonState::Processing => "content-loading-symbolic",
        }
        .into()
    }

    fn status(&self) -> ksni::Status {
        ksni::Status::Active
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let description = match self.model.state {
            DaemonState::Idle => "Idle - click to start recording",
            DaemonState::Recording => "Recording - click to confirm",
            DaemonState::Processing => "Processing transcription...",
        };
        ksni::ToolTip {
            icon_name: self.icon_name(),
            icon_pixmap: vec![],
            title: "Voice Dictation".into(),
            description: description.into(),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let cmd = match self.model.state {
            DaemonState::Idle => DaemonCommand::StartRecording,
            DaemonState::Recording => DaemonCommand::Confirm,
            DaemonState::Processing => return,
        };
        if let Err(e) = self.command_tx.try_send(cmd) {
            warn!("Tray activate: failed to send command: {e}");
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        self.model
            .entries()
            .into_iter()
            .map(|entry| match entry {
                TrayEntry::Item { label, enabled, action } => StandardItem {
                    label: label.into(),
                    enabled,
                    activate: Box::new(move |tray: &mut Self| {
                        tray.model.perform(action.clone(), &tray.command_tx)
                    }),
                    ..Default::default()
                }
                .into(),
                TrayEntry::Devices { label, options, selected } => {
                    info!(
                        "Tray menu: {} cached devices, {} radio options, selected_idx={}",
                        self.model.devices.len(),
                        options.len(),
                        selected
                    );
                    SubMenu {
                        label: label.into(),
                        submenu: vec![RadioGroup {
                            selected,
                            select: Box::new(|tray: &mut Self, index: usize| {
                                tray.model
                                    .perform(TrayAction::SelectDevice(index), &tray.command_tx)
                            }),
                            options: options
                                .into_iter()
                                .map(|label| RadioItem { label, ..Default::default() })
                                .collect(),
                        }
                        .into()],
                        ..Default::default()
                    }
                    .into()
                }
                TrayEntry::Separator => MenuItem::Separator,
            })
            .collect()
    }

    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        warn!("Tray host went offline, keeping alive for reconnection");
        true
    }
}

pub fn spawn_tray(
    state_rx: watch::Receiver<DaemonState>,
    command_tx: mpsc::Sender<DaemonCommand>,
    backend_type: BackendType,
    initial_device: Option<String>,
) -> Option<tokio::task::JoinHandle<()>> {
    Some(tokio::spawn(async move {
        let mut backoff = std::time::Duration::from_secs(5);
        let max_backoff = std::time::Duration::from_secs(30);

        loop {
            let initial_devices =
                crate::audio_backend::list_devices(backend_type).unwrap_or_default();

            let tray = DictationTray {
                model: TrayModel {
                    state: *state_rx.borrow(),
                    devices: initial_devices,
                    selected_device: initial_device.clone(),
                },
                command_tx: command_tx.clone(),
                icon_invalidated: false,
            };

            match tray.spawn().await {
                Ok(handle) => {
                    info!("System tray icon active");

                    let theme_handle = handle.clone();
                    tokio::spawn(async move {
                        if let Err(e) = listen_theme_changes(theme_handle).await {
                            warn!("Theme change listener error: {e}");
                        }
                    });

                    run_tray_update_loop(handle, state_rx, backend_type).await;
                    break;
                }
                Err(e) => {
                    warn!(
                        "Failed to spawn system tray (no tray host?): {e}, retrying in {backoff:?}"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
        }
    }))
}

async fn run_tray_update_loop(
    handle: Handle<DictationTray>,
    mut state_rx: watch::Receiver<DaemonState>,
    backend_type: BackendType,
) {
    let mut refresh_interval = tokio::time::interval(std::time::Duration::from_secs(30));
    refresh_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    refresh_interval.tick().await;

    loop {
        tokio::select! {
            result = state_rx.changed() => {
                if result.is_err() {
                    break;
                }
                let new_state = *state_rx.borrow_and_update();
                let refresh = new_state == DaemonState::Idle;
                let bt = backend_type;
                if handle
                    .update(move |tray| {
                        tray.model.state = new_state;
                        if refresh {
                            if let Ok(devs) = crate::audio_backend::list_devices(bt) {
                                tray.model.devices = devs;
                            }
                        }
                    })
                    .await
                    .is_none()
                {
                    break;
                }
            }
            _ = refresh_interval.tick() => {
                let bt = backend_type;
                if handle
                    .update(move |tray| {
                        if let Ok(devs) = crate::audio_backend::list_devices(bt) {
                            tray.model.devices = devs;
                        }
                    })
                    .await
                    .is_none()
                {
                    break;
                }
            }
        }
    }
}

async fn listen_theme_changes(handle: Handle<DictationTray>) -> anyhow::Result<()> {
    let connection = zbus::Connection::session().await?;

    let match_rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("ca.desrt.dconf.Writer")?
        .member("Notify")?
        .build();

    let proxy = zbus::fdo::DBusProxy::new(&connection).await?;
    proxy.add_match_rule(match_rule).await?;

    let mut stream = zbus::MessageStream::from(&connection);

    info!("Listening for icon-theme changes");

    while let Some(msg) = stream.next().await {
        if let Ok(msg) = msg {
            if let Some(member) = msg.header().member() {
                if member.as_str() == "Notify" {
                    if let Ok((path, _, _)) = msg.body().deserialize::<(&str, Vec<&str>, &str)>() {
                        if path.contains("desktop/interface") {
                            info!("Icon theme changed, debouncing before refresh");
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            while let Ok(Some(_)) = tokio::time::timeout(
                                std::time::Duration::from_millis(50),
                                stream.next(),
                            )
                            .await
                            {}
                            handle
                                .update(|tray| {
                                    tray.icon_invalidated = true;
                                })
                                .await;
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            handle
                                .update(|tray| {
                                    tray.icon_invalidated = false;
                                })
                                .await;
                            info!("Tray icon refreshed");
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
