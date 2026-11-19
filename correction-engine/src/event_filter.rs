use crate::types::{EventSource, InjectionContext, MonitorConfig, TextChangeEvent, TextChangeOp};
use std::time::Duration;

pub fn filter_events(
    events: &[TextChangeEvent],
    context: &InjectionContext,
    config: &MonitorConfig,
) -> Vec<TextChangeEvent> {
    let injection_len = context.text.chars().count() as i32;

    filter_app_events(events, context, config)
        .into_iter()
        .filter(|event| {
            let event_end = event.start_pos + event.length;
            event.start_pos <= injection_len && event_end >= 0
        })
        .collect()
}

pub fn filter_field_events(
    events: &[TextChangeEvent],
    context: &InjectionContext,
    config: &MonitorConfig,
) -> Vec<TextChangeEvent> {
    let mut dictated_end_limit = context.text.chars().count();

    filter_app_events(events, context, config)
        .into_iter()
        .filter(|event| {
            let within =
                usize::try_from(event.start_pos).is_ok_and(|start| start <= dictated_end_limit);
            if within && event.operation == TextChangeOp::Insert {
                dictated_end_limit += event.text.chars().count();
            }
            within
        })
        .collect()
}

fn filter_app_events(
    events: &[TextChangeEvent],
    context: &InjectionContext,
    config: &MonitorConfig,
) -> Vec<TextChangeEvent> {
    let monitor_duration = Duration::from_secs(config.monitor_duration_secs);

    events
        .iter()
        .filter(|event| event.timestamp.duration_since(context.instant) <= monitor_duration)
        .filter(|event| from_dictated_app(&event.source, context))
        .cloned()
        .collect()
}

fn from_dictated_app(source: &EventSource, context: &InjectionContext) -> bool {
    match source {
        EventSource::Process(pid) => match context.window_pid {
            Some(focused) => focused == *pid,
            None => context.window_class.is_empty(),
        },
        EventSource::App(app) => {
            let source_lower = app.to_lowercase();
            let target_lower = context.window_class.to_lowercase();
            target_lower.is_empty()
                || source_lower.contains(&target_lower)
                || target_lower.contains(&source_lower)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TextChangeOp;
    use chrono::Utc;
    use std::path::PathBuf;
    use std::time::Instant;

    fn make_config(duration_secs: u64) -> MonitorConfig {
        MonitorConfig {
            enabled: true,
            monitor_duration_secs: duration_secs,
            auto_promote_threshold: 3,
            max_age_days: 30,
            store_path: PathBuf::from("/tmp/test-corrections.json"),
            substitutions_path: PathBuf::from("/tmp/test-substitutions.txt"),
        }
    }

    fn make_context(text: &str, window_class: &str) -> (InjectionContext, Instant) {
        let base = Instant::now();
        let ctx = InjectionContext {
            text: text.to_string(),
            timestamp: Utc::now(),
            instant: base,
            window_class: window_class.to_string(),
            window_pid: None,
            window_title: "Test".to_string(),
        };
        (ctx, base)
    }

    fn make_event(
        base: Instant,
        offset: Duration,
        pos: i32,
        len: i32,
        app: &str,
    ) -> TextChangeEvent {
        TextChangeEvent {
            operation: TextChangeOp::Insert,
            start_pos: pos,
            length: len,
            text: "x".repeat(len as usize),
            timestamp: base + offset,
            source: EventSource::App(app.to_string()),
        }
    }

    #[test]
    fn test_time_window_filter() {
        let (context, base) = make_context("hello world", "firefox");
        let config = make_config(60);

        let events = vec![
            make_event(base, Duration::from_secs(5), 3, 1, "firefox"),
            make_event(base, Duration::from_secs(30), 3, 1, "firefox"),
            make_event(base, Duration::from_secs(90), 3, 1, "firefox"),
        ];

        let filtered = filter_events(&events, &context, &config);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_position_filter() {
        let (context, base) = make_context("hello world", "firefox");
        let config = make_config(300);

        let events = vec![
            make_event(base, Duration::from_secs(1), 5, 1, "firefox"),
            make_event(base, Duration::from_secs(2), 15, 1, "firefox"),
            make_event(base, Duration::from_secs(3), 25, 1, "firefox"),
        ];

        let filtered = filter_events(&events, &context, &config);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].start_pos, 5);
    }

    #[test]
    fn test_app_filter() {
        let (context, base) = make_context("hello world", "firefox");
        let config = make_config(300);

        let events = vec![
            make_event(base, Duration::from_secs(1), 3, 1, "firefox"),
            make_event(base, Duration::from_secs(2), 3, 1, "code"),
            make_event(base, Duration::from_secs(3), 3, 1, "Firefox-esr"),
        ];

        let filtered = filter_events(&events, &context, &config);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_empty_window_class_accepts_all() {
        let (context, base) = make_context("hello world", "");
        let config = make_config(300);

        let events = vec![
            make_event(base, Duration::from_secs(1), 3, 1, "firefox"),
            make_event(base, Duration::from_secs(2), 3, 1, "code"),
        ];

        let filtered = filter_events(&events, &context, &config);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_combined_filters() {
        let (context, base) = make_context("hello world", "firefox");
        let config = make_config(60);

        let events = vec![
            make_event(base, Duration::from_secs(5), 3, 1, "firefox"),
            make_event(base, Duration::from_secs(90), 3, 1, "firefox"),
            make_event(base, Duration::from_secs(5), 50, 1, "firefox"),
            make_event(base, Duration::from_secs(5), 3, 1, "code"),
            make_event(base, Duration::from_secs(10), 6, 2, "Firefox"),
        ];

        let filtered = filter_events(&events, &context, &config);
        assert_eq!(filtered.len(), 2);
    }
}
