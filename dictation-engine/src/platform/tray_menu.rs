use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::audio_backend::DeviceInfo;
use crate::control::{DaemonCommand, DaemonState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayAction {
    Daemon(DaemonCommand),
    SelectDevice(usize),
    OpenTestLoop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayEntry {
    Item { label: &'static str, enabled: bool, action: TrayAction },
    Devices { label: &'static str, options: Vec<String>, selected: usize },
    Separator,
}

pub struct TrayModel {
    pub state: DaemonState,
    pub devices: Vec<DeviceInfo>,
    pub selected_device: Option<String>,
}

impl TrayModel {
    pub fn entries(&self) -> Vec<TrayEntry> {
        let is_idle = self.state == DaemonState::Idle;
        vec![
            TrayEntry::Item {
                label: if is_idle { "Start Recording" } else { "Confirm" },
                enabled: self.state != DaemonState::Processing,
                action: TrayAction::Daemon(if is_idle {
                    DaemonCommand::StartRecording
                } else {
                    DaemonCommand::Confirm
                }),
            },
            TrayEntry::Item {
                label: "Cancel",
                enabled: self.state == DaemonState::Recording,
                action: TrayAction::Daemon(DaemonCommand::StopRecording),
            },
            TrayEntry::Separator,
            TrayEntry::Devices {
                label: "Input Device",
                options: std::iter::once("Default".to_string())
                    .chain(self.devices.iter().map(|device| device.description.clone()))
                    .collect(),
                selected: self.selected_index(),
            },
            TrayEntry::Separator,
            TrayEntry::Item {
                label: "Open Test Loop",
                enabled: true,
                action: TrayAction::OpenTestLoop,
            },
            TrayEntry::Separator,
            TrayEntry::Item {
                label: "Restart Daemon",
                enabled: true,
                action: TrayAction::Daemon(DaemonCommand::Restart),
            },
            TrayEntry::Item {
                label: "Quit",
                enabled: true,
                action: TrayAction::Daemon(DaemonCommand::Shutdown),
            },
        ]
    }

    fn selected_index(&self) -> usize {
        self.selected_device
            .as_ref()
            .and_then(|name| self.devices.iter().position(|device| &device.name == name))
            .map_or(0, |position| position + 1)
    }

    fn select_device(&mut self, index: usize) -> DaemonCommand {
        let device = index
            .checked_sub(1)
            .and_then(|position| self.devices.get(position))
            .map(|d| d.name.clone());
        info!("Tray: Selected device {:?}", device.as_deref().unwrap_or("Default"));
        self.selected_device = device.clone();
        DaemonCommand::SwitchDevice(device)
    }

    pub fn perform(&mut self, action: TrayAction, commands: &mpsc::Sender<DaemonCommand>) {
        let command = match action {
            TrayAction::Daemon(command) => command,
            TrayAction::SelectDevice(index) => self.select_device(index),
            TrayAction::OpenTestLoop => {
                if let Ok(exe) = std::env::current_exe() {
                    let _ = std::process::Command::new(exe).arg("test-loop").spawn();
                }
                return;
            }
        };
        if let Err(e) = commands.try_send(command) {
            warn!("Tray: failed to send command: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str, description: &str) -> DeviceInfo {
        DeviceInfo {
            name: name.to_string(),
            description: description.to_string(),
            is_default: false,
        }
    }

    fn model(state: DaemonState) -> TrayModel {
        TrayModel {
            state,
            devices: vec![device("usb-mic", "USB Microphone"), device("builtin", "Built-in")],
            selected_device: None,
        }
    }

    fn items(model: &TrayModel) -> Vec<(&'static str, bool, TrayAction)> {
        model
            .entries()
            .into_iter()
            .filter_map(|entry| match entry {
                TrayEntry::Item { label, enabled, action } => Some((label, enabled, action)),
                _ => None,
            })
            .collect()
    }

    fn sent(model: &mut TrayModel, action: TrayAction) -> Option<DaemonCommand> {
        let (commands, mut received) = mpsc::channel(1);
        model.perform(action, &commands);
        received.try_recv().ok()
    }

    #[test]
    fn the_menu_keeps_one_layout_in_every_state() {
        for state in [DaemonState::Idle, DaemonState::Recording, DaemonState::Processing] {
            let shape: Vec<&str> = model(state)
                .entries()
                .iter()
                .map(|entry| match entry {
                    TrayEntry::Item { label, .. } => *label,
                    TrayEntry::Devices { label, .. } => *label,
                    TrayEntry::Separator => "-",
                })
                .collect();
            let primary = if state == DaemonState::Idle { "Start Recording" } else { "Confirm" };
            assert_eq!(
                shape,
                [
                    primary,
                    "Cancel",
                    "-",
                    "Input Device",
                    "-",
                    "Open Test Loop",
                    "-",
                    "Restart Daemon",
                    "Quit"
                ]
            );
        }
    }

    #[test]
    fn idle_offers_start_and_cannot_cancel() {
        let entries = items(&model(DaemonState::Idle));
        assert_eq!(
            entries[0],
            ("Start Recording", true, TrayAction::Daemon(DaemonCommand::StartRecording))
        );
        assert_eq!(entries[1], ("Cancel", false, TrayAction::Daemon(DaemonCommand::StopRecording)));
    }

    #[test]
    fn recording_offers_confirm_and_cancel() {
        let entries = items(&model(DaemonState::Recording));
        assert_eq!(entries[0], ("Confirm", true, TrayAction::Daemon(DaemonCommand::Confirm)));
        assert_eq!(entries[1], ("Cancel", true, TrayAction::Daemon(DaemonCommand::StopRecording)));
    }

    #[test]
    fn processing_disables_confirm_and_cancel() {
        let entries = items(&model(DaemonState::Processing));
        assert_eq!(entries[0], ("Confirm", false, TrayAction::Daemon(DaemonCommand::Confirm)));
        assert!(!entries[1].1);
    }

    #[test]
    fn restart_and_quit_are_always_available() {
        for state in [DaemonState::Idle, DaemonState::Recording, DaemonState::Processing] {
            let entries = items(&model(state));
            assert!(entries.contains(&(
                "Restart Daemon",
                true,
                TrayAction::Daemon(DaemonCommand::Restart)
            )));
            assert!(entries.contains(&("Quit", true, TrayAction::Daemon(DaemonCommand::Shutdown))));
            assert!(entries.contains(&("Open Test Loop", true, TrayAction::OpenTestLoop)));
        }
    }

    fn devices_entry(model: &TrayModel) -> (Vec<String>, usize) {
        model
            .entries()
            .into_iter()
            .find_map(|entry| match entry {
                TrayEntry::Devices { options, selected, .. } => Some((options, selected)),
                _ => None,
            })
            .expect("device submenu")
    }

    #[test]
    fn devices_are_listed_after_default_with_the_configured_one_selected() {
        let mut model = model(DaemonState::Idle);
        assert_eq!(
            devices_entry(&model),
            (vec!["Default".to_string(), "USB Microphone".to_string(), "Built-in".to_string()], 0)
        );

        model.selected_device = Some("builtin".to_string());
        assert_eq!(devices_entry(&model).1, 2);

        model.selected_device = Some("unplugged".to_string());
        assert_eq!(devices_entry(&model).1, 0);
    }

    #[test]
    fn selecting_a_device_switches_the_daemon_and_moves_the_mark() {
        let mut model = model(DaemonState::Idle);

        assert_eq!(
            sent(&mut model, TrayAction::SelectDevice(1)),
            Some(DaemonCommand::SwitchDevice(Some("usb-mic".to_string())))
        );
        assert_eq!(devices_entry(&model).1, 1);

        assert_eq!(
            sent(&mut model, TrayAction::SelectDevice(0)),
            Some(DaemonCommand::SwitchDevice(None))
        );
        assert_eq!(devices_entry(&model).1, 0);
    }

    #[test]
    fn a_menu_command_reaches_the_daemon() {
        let mut model = model(DaemonState::Recording);
        assert_eq!(
            sent(&mut model, TrayAction::Daemon(DaemonCommand::Confirm)),
            Some(DaemonCommand::Confirm)
        );
    }
}
