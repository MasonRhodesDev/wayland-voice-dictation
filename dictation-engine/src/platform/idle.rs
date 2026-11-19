use anyhow::Result;
use tracing::info;

pub struct IdleInhibitor {
    _awake: keepawake::KeepAwake,
}

pub async fn inhibit_idle(reason: &str) -> Result<IdleInhibitor> {
    let reason = reason.to_string();
    let awake = tokio::task::spawn_blocking(move || {
        keepawake::Builder::default()
            .idle(true)
            .sleep(cfg!(target_os = "linux"))
            .reason(reason)
            .app_name("voice-dictation")
            .create()
    })
    .await??;
    info!("Idle inhibit acquired");
    Ok(IdleInhibitor { _awake: awake })
}
