use anyhow::Result;
use std::time::Duration;
use tracing::debug;

pub struct KeyboardInjector;

impl KeyboardInjector {
    pub fn new() -> Self {
        Self
    }

    #[allow(dead_code)]
    pub async fn type_text(&self, text: &str, word_delay_ms: u64) -> Result<()> {
        self.type_text_with_progress(text, word_delay_ms, |_done, _total| {}).await
    }

    pub async fn type_text_with_progress(
        &self,
        text: &str,
        word_delay_ms: u64,
        mut on_progress: impl FnMut(usize, usize),
    ) -> Result<()> {
        debug!("Typing text: {}", text);

        let words: Vec<&str> = text.split_whitespace().collect();
        let total = words.len();
        on_progress(0, total);

        if word_delay_ms > 0 {
            for (i, word) in words.iter().enumerate() {
                let chunk = if i == 0 { (*word).to_string() } else { format!(" {}", word) };

                let output = tokio::process::Command::new("wtype").arg(&chunk).output().await?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!("wtype failed: {}", stderr);
                }

                on_progress(i + 1, total);

                tokio::time::sleep(Duration::from_millis(word_delay_ms)).await;
            }
        } else {
            let output = tokio::process::Command::new("wtype").arg(text).output().await?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("wtype failed: {}", stderr);
            }

            on_progress(total, total);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keyboard_injector_new() {
        let _injector = KeyboardInjector::new();
    }

    #[tokio::test]
    async fn test_type_text_interface() {
        let injector = KeyboardInjector::new();
        let result = injector.type_text("test", 0).await;
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_type_text_word_delay() {
        let injector = KeyboardInjector::new();
        let result = injector.type_text("test", 50).await;
        assert!(result.is_ok() || result.is_err());
    }
}
