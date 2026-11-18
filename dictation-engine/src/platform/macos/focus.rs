use std::time::Duration;

use anyhow::{bail, Result};
use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
use tracing::debug;

use crate::platform::lsappinfo::field_value;

pub struct FocusTarget {
    pid: libc::pid_t,
    class: String,
}

async fn lsappinfo(args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new("lsappinfo").args(args).output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

impl FocusTarget {
    pub async fn capture() -> Option<Self> {
        let front = lsappinfo(&["front"]).await?;
        let asn = front.trim();
        let pid: libc::pid_t =
            field_value(&lsappinfo(&["info", "-only", "pid", asn]).await?)?.parse().ok()?;
        let class =
            field_value(&lsappinfo(&["info", "-only", "bundleid", asn]).await?)?.to_string();
        debug!("Captured frontmost app: class={}, pid={}", class, pid);
        Some(Self { pid, class })
    }

    pub async fn refocus(&self) -> Result<()> {
        let activated = NSRunningApplication::runningApplicationWithProcessIdentifier(self.pid)
            .is_some_and(|app| app.activateWithOptions(NSApplicationActivationOptions::empty()));
        if !activated {
            bail!("Could not activate {} (pid {})", self.class, self.pid);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(())
    }

    pub fn class(&self) -> &str {
        &self.class
    }
}
