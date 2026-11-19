use std::time::Duration;

use anyhow::{anyhow, Result};
use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use tracing::debug;

use crate::platform::utf16::utf16_chunks;

const MAX_UTF16_PER_EVENT: usize = 20;
const CHUNK_DELAY: Duration = Duration::from_millis(5);

pub async fn type_text_with_progress(
    text: &str,
    word_delay_ms: u64,
    mut on_progress: impl FnMut(usize, usize),
) -> Result<()> {
    debug!("Typing text: {}", text);
    if !super::permissions::can_post_events() {
        return Err(anyhow!("{}", super::permissions::accessibility()));
    }

    let words: Vec<&str> = text.split_whitespace().collect();
    let total = words.len();
    on_progress(0, total);

    if word_delay_ms > 0 {
        for (i, word) in words.iter().enumerate() {
            let chunk = if i == 0 { (*word).to_string() } else { format!(" {}", word) };
            type_string(&chunk).await?;
            on_progress(i + 1, total);
            tokio::time::sleep(Duration::from_millis(word_delay_ms)).await;
        }
    } else {
        type_string(text).await?;
        on_progress(total, total);
    }

    Ok(())
}

async fn type_string(text: &str) -> Result<()> {
    for chunk in utf16_chunks(text, MAX_UTF16_PER_EVENT) {
        post_chunk(chunk)?;
        tokio::time::sleep(CHUNK_DELAY).await;
    }
    Ok(())
}

fn post_chunk(chunk: &str) -> Result<()> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|()| anyhow!("Failed to create a CGEventSource"))?;
    for key_down in [true, false] {
        let event = CGEvent::new_keyboard_event(source.clone(), 0, key_down)
            .map_err(|()| anyhow!("Failed to create a keyboard CGEvent"))?;
        event.set_flags(CGEventFlags::CGEventFlagNull);
        event.set_string(chunk);
        event.post(CGEventTapLocation::HID);
    }
    Ok(())
}
