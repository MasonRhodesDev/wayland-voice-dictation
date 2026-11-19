use anyhow::Result;
use tracing::info;
use zbus::Connection;

pub struct IdleInhibitor {
    _inhibit: logind_session::Inhibitor,
}

pub async fn inhibit_idle(reason: &str) -> Result<IdleInhibitor> {
    let conn = Connection::system().await?;
    let proxy = logind_session::LogindManagerProxy::new(&conn).await?;
    let inhibit = logind_session::Inhibitor::acquire(
        &proxy,
        "idle:sleep",
        "voice-dictation",
        reason,
        "block",
    )
    .await?;
    info!("Idle/sleep inhibit acquired via logind");
    Ok(IdleInhibitor { _inhibit: inhibit })
}
