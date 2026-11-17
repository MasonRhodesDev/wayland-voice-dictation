use dictation_engine::audio_backend::{BackendType, DeviceInfo};

pub fn list_models() -> Vec<String> {
    vec![
        "parakeet:default".to_string(),
        "openai:gpt-live-transcribe".to_string(),
        "openai:gpt-transcribe".to_string(),
        "openai:whisper-1".to_string(),
    ]
}

pub fn list_preview_models(_language: &str) -> Vec<String> {
    list_models()
}

pub fn list_final_models(_language: &str) -> Vec<String> {
    list_models()
}

pub fn list_audio_devices() -> Vec<DeviceInfo> {
    dictation_engine::audio_backend::list_devices(BackendType::Auto).unwrap_or_default()
}

pub fn get_engine_summary() -> String {
    "parakeet".to_string()
}
