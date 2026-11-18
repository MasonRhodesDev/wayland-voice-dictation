use block2::RcBlock;
use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::string::{CFString, CFStringRef};
use objc2::runtime::Bool;
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
use tracing::info;

use crate::platform::Permission;

#[allow(non_upper_case_globals)]
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    static kAXTrustedCheckOptionPrompt: CFStringRef;
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> u8;
}

fn missing(name: &'static str) -> Permission {
    let binary = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "voice-dictation".to_string());
    Permission {
        name,
        remedy: format!("allow {} in System Settings > Privacy & Security > {}", binary, name),
    }
}

pub(super) fn accessibility() -> Permission {
    missing("Accessibility")
}

pub(super) fn automation() -> Permission {
    missing("Automation")
}

pub(super) fn accessibility_granted() -> bool {
    unsafe { AXIsProcessTrusted() != 0 }
}

fn microphone_status() -> Option<AVAuthorizationStatus> {
    let audio = unsafe { AVMediaTypeAudio }?;
    Some(unsafe { AVCaptureDevice::authorizationStatusForMediaType(audio) })
}

pub fn missing_permissions() -> Vec<Permission> {
    let mut missing_now = Vec::new();
    if matches!(
        microphone_status(),
        Some(AVAuthorizationStatus::Denied | AVAuthorizationStatus::Restricted)
    ) {
        missing_now.push(missing("Microphone"));
    }
    if !accessibility_granted() {
        missing_now.push(accessibility());
    }
    missing_now
}

pub fn request_permissions() -> Vec<Permission> {
    if microphone_status() == Some(AVAuthorizationStatus::NotDetermined) {
        if let Some(audio) = unsafe { AVMediaTypeAudio } {
            let on_answer = RcBlock::new(|granted: Bool| {
                info!("Microphone access granted: {}", granted.as_bool());
            });
            unsafe {
                AVCaptureDevice::requestAccessForMediaType_completionHandler(audio, &on_answer)
            };
        }
    }

    let prompt = unsafe { CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt) };
    let options = CFDictionary::from_CFType_pairs(&[(prompt, CFBoolean::true_value())]);
    unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) };

    missing_permissions()
}
