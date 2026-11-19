use anyhow::Result;
use std::time::Duration;
use tracing::debug;

pub async fn type_text_with_progress(
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
