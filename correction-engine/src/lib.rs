pub mod connection;
pub mod diff;
pub mod event_filter;
pub mod store;
pub mod types;
pub mod wezterm;

pub use connection::{AtspiConnection, MockTextChangeSource, TextChangeSource};
pub use store::CorrectionStore;
pub use types::{
    CorrectionPair, CorrectionStats, EventScope, EventSource, InjectionContext, MonitorConfig,
    TextChangeEvent,
};
pub use wezterm::{WeztermMonitor, WEZTERM_WINDOW_CLASS};

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

pub struct CorrectionMonitor {
    source: Arc<dyn TextChangeSource>,
    store: Arc<Mutex<CorrectionStore>>,
    config: MonitorConfig,
}

impl CorrectionMonitor {
    pub fn with_source(
        source: Arc<dyn TextChangeSource>,
        store: CorrectionStore,
        config: MonitorConfig,
    ) -> Self {
        Self { source, store: Arc::new(Mutex::new(store)), config }
    }

    pub fn start_monitoring(
        &self,
        context: InjectionContext,
    ) -> tokio::task::JoinHandle<Vec<CorrectionPair>> {
        let source = Arc::clone(&self.source);
        let store = Arc::clone(&self.store);
        let config = self.config.clone();

        tokio::spawn(async move {
            match Self::run_monitoring(source, store, config, context).await {
                Ok(corrections) => corrections,
                Err(e) => {
                    error!("Correction monitoring failed: {}", e);
                    Vec::new()
                }
            }
        })
    }

    async fn run_monitoring(
        source: Arc<dyn TextChangeSource>,
        store: Arc<Mutex<CorrectionStore>>,
        config: MonitorConfig,
        context: InjectionContext,
    ) -> Result<Vec<CorrectionPair>> {
        if !source.is_available() {
            debug!("AT-SPI2 not available, skipping correction monitoring");
            return Ok(Vec::new());
        }

        let mut rx = source.subscribe(&context).await?;
        let monitor_duration = Duration::from_secs(config.monitor_duration_secs);

        info!(
            "Correction monitoring started for {}s (text: '{}...')",
            config.monitor_duration_secs,
            &context.text[..context.text.len().min(30)]
        );

        let mut events = Vec::new();
        let deadline = tokio::time::Instant::now() + monitor_duration;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }

            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(event)) => {
                    debug!(
                        "AT-SPI2 event: {:?} at pos {} text='{}' from '{}'",
                        event.operation, event.start_pos, event.text, event.source
                    );
                    events.push(event);
                }
                Ok(None) => {
                    debug!("Event channel closed, ending monitoring early");
                    break;
                }
                Err(_) => {
                    break;
                }
            }
        }

        info!("Correction monitoring ended. {} events collected.", events.len());

        if events.is_empty() {
            return Ok(Vec::new());
        }

        info!("Monitoring window summary: {}", summarize_events(&events));

        let scope = source.event_scope();
        let filtered = match scope {
            EventScope::Field => event_filter::filter_field_events(&events, &context, &config),
            EventScope::DictatedText => event_filter::filter_events(&events, &context, &config),
        };
        debug!("After filtering: {} of {} events are relevant", filtered.len(), events.len());

        if filtered.is_empty() {
            return Ok(Vec::new());
        }

        let corrections = match scope {
            EventScope::Field => diff::extract_field_corrections(&context, &filtered),
            EventScope::DictatedText => diff::extract_corrections(&context, &filtered),
        };

        if corrections.is_empty() {
            debug!("No corrections extracted from events");
            return Ok(Vec::new());
        }

        info!("Detected {} correction(s)", corrections.len());

        let mut store = store.lock().await;
        for correction in &corrections {
            match store.record_correction(correction.clone()) {
                Ok(promoted) => {
                    if promoted {
                        info!(
                            "Correction auto-promoted to substitution: '{}' → '{}'",
                            correction.original, correction.corrected
                        );
                    }
                }
                Err(e) => {
                    warn!("Failed to record correction: {}", e);
                }
            }
        }

        Ok(corrections)
    }

    pub fn is_available(&self) -> bool {
        self.source.is_available()
    }

    pub fn store_handle(&self) -> Arc<Mutex<CorrectionStore>> {
        Arc::clone(&self.store)
    }

    pub async fn stats(&self) -> CorrectionStats {
        let store = self.store.lock().await;
        store.stats()
    }
}

fn summarize_events(events: &[TextChangeEvent]) -> String {
    let mut per_app: std::collections::BTreeMap<&EventSource, (usize, usize)> =
        std::collections::BTreeMap::new();
    for event in events {
        let entry = per_app.entry(&event.source).or_insert((0, 0));
        match event.operation {
            types::TextChangeOp::Insert => entry.0 += 1,
            types::TextChangeOp::Delete => entry.1 += 1,
        }
    }
    per_app
        .iter()
        .map(|(app, (inserts, deletes))| {
            format!("'{}': {} insert(s), {} delete(s)", app, inserts, deletes)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TextChangeOp;
    use std::time::Instant;

    fn make_event(source_app: &str, operation: TextChangeOp) -> TextChangeEvent {
        TextChangeEvent {
            operation,
            start_pos: 0,
            length: 1,
            text: "x".to_string(),
            timestamp: Instant::now(),
            source: EventSource::App(source_app.to_string()),
        }
    }

    #[test]
    fn test_summarize_events_groups_by_app_and_type() {
        let events = vec![
            make_event("firefox", TextChangeOp::Insert),
            make_event("firefox", TextChangeOp::Insert),
            make_event("firefox", TextChangeOp::Delete),
            make_event("Code", TextChangeOp::Insert),
        ];

        assert_eq!(
            summarize_events(&events),
            "'Code': 1 insert(s), 0 delete(s); 'firefox': 2 insert(s), 1 delete(s)"
        );
    }
}
