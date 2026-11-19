use rust_platform_kit::permissions::{self as kit, Access, Permission as Gate};

use crate::platform::Permission;

const NEEDED: [Gate; 2] = [Gate::Microphone, Gate::Accessibility];

fn program() -> String {
    std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "voice-dictation".to_string())
}

fn describe(gate: Gate) -> Permission {
    Permission { name: gate.settings_pane(), remedy: gate.remedy(&program()) }
}

pub(super) fn accessibility() -> Permission {
    describe(Gate::Accessibility)
}

pub(super) fn automation() -> Permission {
    Permission {
        name: "Automation",
        remedy: format!("allow {} in System Settings > Privacy & Security > Automation", program()),
    }
}

pub(super) fn accessibility_granted() -> bool {
    kit::access(Gate::Accessibility) == Access::Granted
}

pub(super) fn can_post_events() -> bool {
    kit::access(Gate::PostEvents) == Access::Granted
}

pub fn missing_permissions() -> Vec<Permission> {
    NEEDED.into_iter().filter(|gate| kit::access(*gate) == Access::Denied).map(describe).collect()
}

pub fn request_permissions() -> Vec<Permission> {
    for gate in NEEDED {
        kit::request(gate);
    }
    missing_permissions()
}
