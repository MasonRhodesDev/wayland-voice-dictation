use anyhow::Result;
use std::time::Duration;
use tracing::debug;

pub struct FocusTarget {
    address: String,
    class: String,
    pid: Option<u32>,
}

impl FocusTarget {
    pub async fn capture() -> Option<Self> {
        let output = tokio::process::Command::new("hyprctl")
            .args(["activewindow", "-j"])
            .output()
            .await
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let target = Self::from_active_window(&String::from_utf8(output.stdout).ok()?)?;
        debug!(
            "Captured window: class={}, address={}, pid={:?}",
            target.class, target.address, target.pid
        );
        Some(target)
    }

    fn from_active_window(json: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(json).ok()?;

        let address = value["address"].as_str()?.to_string();
        let class = value["class"].as_str()?.to_string();
        let pid =
            value["pid"].as_u64().and_then(|pid| u32::try_from(pid).ok()).filter(|&pid| pid != 0);

        Some(Self { address, class, pid })
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

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_focused_window_carries_the_pid_of_its_process() {
        let target = FocusTarget::from_active_window(
            r#"{"address": "0x55d0c0ffee00", "class": "firefox", "title": "Mozilla Firefox", "pid": 4242}"#,
        )
        .unwrap();

        assert_eq!(target.class(), "firefox");
        assert_eq!(target.pid(), Some(4242));
    }

    #[test]
    fn a_window_without_a_process_id_is_still_a_focus_target() {
        for pid in ["-1", "0", "null"] {
            let json =
                format!(r#"{{"address": "0x55d0c0ffee00", "class": "steam", "pid": {pid}}}"#);
            let target = FocusTarget::from_active_window(&json).unwrap();

            assert_eq!(target.class(), "steam");
            assert_eq!(target.pid(), None);
        }
    }
}
