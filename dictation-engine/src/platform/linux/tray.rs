use futures_util::StreamExt;
use ksni::menu::{MenuItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{Handle, Tray, TrayMethods};
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::audio_backend::BackendType;
use crate::control::{DaemonCommand, DaemonState};

pub struct DictationTray {
    state: DaemonState,
    command_tx: mpsc::Sender<DaemonCommand>,
    cached_devices: Vec<crate::audio_backend::DeviceInfo>,
    selected_device: Option<String>,
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
        match self.state {
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
        let description = match self.state {
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
        let cmd = match self.state {
            DaemonState::Idle => DaemonCommand::StartRecording,
            DaemonState::Recording => DaemonCommand::Confirm,
            DaemonState::Processing => return,
        };
        if let Err(e) = self.command_tx.try_send(cmd) {
            warn!("Tray activate: failed to send command: {e}");
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let is_idle = self.state == DaemonState::Idle;
        let is_recording = self.state == DaemonState::Recording;

        let devices = &self.cached_devices;

        let mut device_names: Vec<Option<String>> = vec![None];
        for dev in devices {
            device_names.push(Some(dev.name.clone()));
        }

        let selected_idx = match &self.selected_device {
            None => 0,
            Some(name) => device_names.iter().position(|d| d.as_deref() == Some(name)).unwrap_or(0),
        };

        let mut radio_options = vec![RadioItem { label: "Default".into(), ..Default::default() }];
        for dev in devices {
            radio_options.push(RadioItem { label: dev.description.clone(), ..Default::default() });
        }

        info!(
            "Tray menu: {} cached devices, {} radio options, selected_idx={}",
            devices.len(),
            radio_options.len(),
            selected_idx
        );

        let device_submenu = SubMenu {
            label: "Input Device".into(),
            submenu: vec![RadioGroup {
                selected: selected_idx,
                select: Box::new(move |tray: &mut Self, index: usize| {
                    let new_device = if index == 0 {
                        None
                    } else {
                        tray.cached_devices.get(index - 1).map(|d| d.name.clone())
                    };
                    info!("Tray: Selected device {:?}", new_device.as_deref().unwrap_or("Default"));
                    tray.selected_device = new_device.clone();
                    if let Err(e) =
                        tray.command_tx.try_send(DaemonCommand::SwitchDevice(new_device))
                    {
                        warn!("Tray: failed to send SwitchDevice: {e}");
                    }
                }),
                options: radio_options,
            }
            .into()],
            ..Default::default()
        };

        vec![
            StandardItem {
                label: if is_idle { "Start Recording".into() } else { "Confirm".into() },
                enabled: !matches!(self.state, DaemonState::Processing),
                activate: Box::new(move |tray: &mut Self| {
                    let cmd = if is_idle {
                        DaemonCommand::StartRecording
                    } else {
                        DaemonCommand::Confirm
                    };
                    if let Err(e) = tray.command_tx.try_send(cmd) {
                        warn!("Tray menu: failed to send command: {e}");
                    }
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Cancel".into(),
                enabled: is_recording,
                activate: Box::new(|tray: &mut Self| {
                    if let Err(e) = tray.command_tx.try_send(DaemonCommand::StopRecording) {
                        warn!("Tray: failed to send StopRecording: {e}");
                    }
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            device_submenu.into(),
            MenuItem::Separator,
            StandardItem {
                label: "Open Test Loop".into(),
                activate: Box::new(|_tray: &mut Self| {
                    if let Ok(exe) = std::env::current_exe() {
                        let _ = std::process::Command::new(exe).arg("test-loop").spawn();
                    }
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Restart Daemon".into(),
                activate: Box::new(|tray: &mut Self| {
                    if let Err(e) = tray.command_tx.try_send(DaemonCommand::Restart) {
                        warn!("Tray: failed to send Restart: {e}");
                    }
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|tray: &mut Self| {
                    if let Err(e) = tray.command_tx.try_send(DaemonCommand::Shutdown) {
                        warn!("Tray: failed to send Shutdown: {e}");
                    }
                }),
                ..Default::default()
            }
            .into(),
        ]
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
                state: *state_rx.borrow(),
                command_tx: command_tx.clone(),
                cached_devices: initial_devices,
                selected_device: initial_device.clone(),
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
                        tray.state = new_state;
                        if refresh {
                            if let Ok(devs) = crate::audio_backend::list_devices(bt) {
                                tray.cached_devices = devs;
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
                            tray.cached_devices = devs;
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
