//! Captures the focused window at recording start and refocuses it before typing.
//! Hyprland-only via hyprctl subprocess calls.

use anyhow::Result;
use std::time::Duration;
use tracing::debug;

pub struct WindowTarget {
    address: String,
    class: String,
    title: String,
    pid: Option<u32>,
}

impl WindowTarget {
    /// Capture the currently focused window. Returns None if hyprctl fails (graceful fallback).
    pub async fn capture() -> Option<Self> {
        let output = tokio::process::Command::new("hyprctl")
            .args(["activewindow", "-j"])
            .output()
            .await
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let json = String::from_utf8(output.stdout).ok()?;
        let value: serde_json::Value = serde_json::from_str(&json).ok()?;

        let address = value["address"].as_str()?.to_string();
        let class = value["class"].as_str()?.to_string();
        let title = value["title"].as_str().unwrap_or_default().to_string();
        let pid = value["pid"].as_u64().and_then(|p| u32::try_from(p).ok());

        debug!("Captured window: class={}, address={}", class, address);
        Some(Self { address, class, title, pid })
    }

    /// Refocus the captured window before typing.
    pub async fn refocus(&self) -> Result<()> {
        let output = tokio::process::Command::new("hyprctl")
            .args(["dispatch", "focuswindow", &format!("address:{}", self.address)])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("hyprctl focuswindow failed: {}", stderr);
        }

        // Brief sleep for compositor to process the focus change
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(())
    }

    pub fn class(&self) -> &str {
        &self.class
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }
}
