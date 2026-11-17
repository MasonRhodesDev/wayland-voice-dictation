use anyhow::Result;
use std::time::Duration;
use tracing::debug;

pub struct WindowTarget {
    address: String,
    class: String,
}

impl WindowTarget {
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

        debug!("Captured window: class={}, address={}", class, address);
        Some(Self { address, class })
    }

    pub async fn refocus(&self) -> Result<()> {
        let output = tokio::process::Command::new("hyprctl")
            .args(["dispatch", "focuswindow", &format!("address:{}", self.address)])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("hyprctl focuswindow failed: {}", stderr);
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(())
    }

    pub fn class(&self) -> &str {
        &self.class
    }
}
