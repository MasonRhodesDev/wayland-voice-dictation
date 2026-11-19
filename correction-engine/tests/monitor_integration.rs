use chrono::Utc;
use correction_engine::connection::MockTextChangeSource;
use correction_engine::store::CorrectionStore;
use correction_engine::types::{
    CorrectionStats, EventScope, InjectionContext, MonitorConfig, TextChangeEvent, TextChangeOp,
};
use correction_engine::CorrectionMonitor;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn make_config(dir: &std::path::Path, duration_secs: u64, threshold: u32) -> MonitorConfig {
    MonitorConfig {
        enabled: true,
        monitor_duration_secs: duration_secs,
        auto_promote_threshold: threshold,
        max_age_days: 30,
        store_path: dir.join("corrections.json"),
        substitutions_path: dir.join("substitutions.txt"),
    }
}

fn make_context(text: &str) -> InjectionContext {
    InjectionContext {
        text: text.to_string(),
        timestamp: Utc::now(),
        instant: Instant::now(),
        window_class: "test-app".to_string(),
        window_title: "Test Window".to_string(),
    }
}

fn make_event(op: TextChangeOp, pos: i32, text: &str) -> TextChangeEvent {
    TextChangeEvent {
        operation: op,
        start_pos: pos,
        length: text.chars().count() as i32,
        text: text.to_string(),
        timestamp: Instant::now(),
        source_app: "test-app".to_string(),
    }
}

fn atspi_context(text: &str) -> InjectionContext {
    InjectionContext { window_class: String::new(), ..make_context(text) }
}

fn atspi_event(op: TextChangeOp, pos: i32, text: &str) -> TextChangeEvent {
    TextChangeEvent { source_app: ":1.3".to_string(), ..make_event(op, pos, text) }
}

fn pair(original: &str, corrected: &str) -> (String, String) {
    (original.to_string(), corrected.to_string())
}

async fn learn(
    config: &MonitorConfig,
    context: InjectionContext,
    events: Vec<TextChangeEvent>,
) -> (Vec<(String, String)>, CorrectionStats) {
    let store = CorrectionStore::load(config).unwrap();
    let (source, tx) = MockTextChangeSource::new(true);
    let monitor = CorrectionMonitor::with_source(Arc::new(source), store, config.clone());
    let handle = monitor.start_monitoring(context);
    for event in events {
        tx.send(event).await.unwrap();
    }
    let pairs = handle.await.unwrap().into_iter().map(|p| (p.original, p.corrected)).collect();
    (pairs, monitor.stats().await)
}

#[tokio::test]
async fn test_full_correction_flow() {
    let dir = TempDir::new().unwrap();
    let config = make_config(dir.path(), 5, 3);
    let store = CorrectionStore::empty(config.clone());
    let (mock_source, tx) = MockTextChangeSource::new(true);

    let monitor = CorrectionMonitor::with_source(Arc::new(mock_source), store, config);

    let context = make_context("hello world");
    let handle = monitor.start_monitoring(context);

    tokio::time::sleep(Duration::from_millis(50)).await;

    tx.send(make_event(TextChangeOp::Delete, 6, "world")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    tx.send(make_event(TextChangeOp::Insert, 6, "earth")).await.unwrap();

    drop(tx);

    let corrections = handle.await.unwrap();
    assert_eq!(corrections.len(), 1);
    assert_eq!(corrections[0].original, "world");
    assert_eq!(corrections[0].corrected, "earth");
}

#[tokio::test]
async fn test_monitoring_timeout() {
    let dir = TempDir::new().unwrap();
    let config = make_config(dir.path(), 1, 3);
    let store = CorrectionStore::empty(config.clone());
    let (mock_source, _tx) = MockTextChangeSource::new(true);

    let monitor = CorrectionMonitor::with_source(Arc::new(mock_source), store, config);

    let context = make_context("hello world");
    let handle = monitor.start_monitoring(context);

    let corrections = handle.await.unwrap();
    assert!(corrections.is_empty());
}

#[tokio::test]
async fn test_graceful_degradation() {
    let dir = TempDir::new().unwrap();
    let config = make_config(dir.path(), 2, 3);
    let store = CorrectionStore::empty(config.clone());
    let (mock_source, _tx) = MockTextChangeSource::new(false);

    let monitor = CorrectionMonitor::with_source(Arc::new(mock_source), store, config);

    assert!(!monitor.is_available());

    let context = make_context("hello world");
    let handle = monitor.start_monitoring(context);

    let corrections = handle.await.unwrap();
    assert!(corrections.is_empty());
}

#[tokio::test]
async fn test_auto_promotion_e2e() {
    let dir = TempDir::new().unwrap();
    let config = make_config(dir.path(), 2, 3);

    for i in 0..3 {
        let store = CorrectionStore::load(&config).unwrap();
        let (mock_source, tx) = MockTextChangeSource::new(true);
        let monitor = CorrectionMonitor::with_source(Arc::new(mock_source), store, config.clone());

        let context = make_context("the cash is here");
        let handle = monitor.start_monitoring(context);

        tokio::time::sleep(Duration::from_millis(50)).await;

        tx.send(make_event(TextChangeOp::Delete, 4, "cash")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        tx.send(make_event(TextChangeOp::Insert, 4, "cache")).await.unwrap();

        drop(tx);
        let corrections = handle.await.unwrap();
        assert_eq!(corrections.len(), 1, "Session {} should detect 1 correction", i);
    }

    let subs_content = std::fs::read_to_string(config.substitutions_path).unwrap();
    assert!(
        subs_content.contains("cash -> cache"),
        "substitutions.txt should contain 'cash -> cache', got: {}",
        subs_content
    );
}

#[tokio::test]
async fn test_stats() {
    let dir = TempDir::new().unwrap();
    let config = make_config(dir.path(), 2, 5);
    let store = CorrectionStore::empty(config.clone());
    let (mock_source, tx) = MockTextChangeSource::new(true);

    let monitor = CorrectionMonitor::with_source(Arc::new(mock_source), store, config);

    let context = make_context("hello world");
    let handle = monitor.start_monitoring(context);

    tokio::time::sleep(Duration::from_millis(50)).await;

    tx.send(make_event(TextChangeOp::Delete, 6, "world")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    tx.send(make_event(TextChangeOp::Insert, 6, "earth")).await.unwrap();

    drop(tx);
    handle.await.unwrap();

    let stats = monitor.stats().await;
    assert_eq!(stats.total_corrections, 1);
    assert_eq!(stats.total_observations, 1);
    assert_eq!(stats.promoted_count, 0);
    assert_eq!(stats.pending_count, 1);
}

#[tokio::test]
async fn a_single_word_fix_is_recorded() {
    let dir = TempDir::new().unwrap();
    let config = make_config(dir.path(), 1, 3);

    let (pairs, stats) = learn(
        &config,
        atspi_context("the cash is here"),
        vec![
            atspi_event(TextChangeOp::Delete, 4, "cash"),
            atspi_event(TextChangeOp::Insert, 4, "cache"),
        ],
    )
    .await;

    assert_eq!(pairs, [pair("cash", "cache")]);
    assert_eq!(stats.total_corrections, 1);
}

#[tokio::test]
async fn a_fix_in_the_focused_window_is_recorded() {
    let dir = TempDir::new().unwrap();
    let config = make_config(dir.path(), 1, 3);
    let context = InjectionContext {
        window_class: "firefox".to_string(),
        ..make_context("the cash is here")
    };

    let (pairs, stats) = learn(
        &config,
        context,
        vec![
            atspi_event(TextChangeOp::Delete, 4, "cash"),
            atspi_event(TextChangeOp::Insert, 4, "cache"),
        ],
    )
    .await;

    assert_eq!(pairs, [pair("cash", "cache")]);
    assert_eq!(stats.total_corrections, 1);
}

#[tokio::test]
async fn sending_a_dictated_chat_message_records_nothing() {
    let dir = TempDir::new().unwrap();
    let config = make_config(dir.path(), 1, 3);
    let dictated = "see you at noon";

    let (pairs, stats) = learn(
        &config,
        atspi_context(dictated),
        vec![atspi_event(TextChangeOp::Delete, 0, dictated)],
    )
    .await;

    assert_eq!(pairs, []);
    assert_eq!(stats.total_corrections, 0);
}

#[tokio::test]
async fn typing_a_new_message_after_sending_records_nothing() {
    let dir = TempDir::new().unwrap();
    let config = make_config(dir.path(), 1, 3);
    let dictated = "see you at noon";

    let (pairs, stats) = learn(
        &config,
        atspi_context(dictated),
        vec![
            atspi_event(TextChangeOp::Delete, 0, dictated),
            atspi_event(TextChangeOp::Insert, 0, "o"),
            atspi_event(TextChangeOp::Insert, 1, "k"),
        ],
    )
    .await;

    assert_eq!(pairs, []);
    assert_eq!(stats.total_corrections, 0);
}

#[tokio::test]
async fn clearing_a_field_that_holds_more_than_the_dictation_records_nothing() {
    let dir = TempDir::new().unwrap();
    let config = make_config(dir.path(), 1, 3);

    let (pairs, stats) = learn(
        &config,
        atspi_context("see you at noon"),
        vec![atspi_event(TextChangeOp::Delete, 0, "see you at noon, bring the keys")],
    )
    .await;

    assert_eq!(pairs, []);
    assert_eq!(stats.total_corrections, 0);
}

#[tokio::test]
async fn a_message_replaced_every_time_is_never_promoted() {
    let dir = TempDir::new().unwrap();
    let config = make_config(dir.path(), 1, 3);

    for _ in 0..3 {
        learn(
            &config,
            atspi_context("okay"),
            vec![
                atspi_event(TextChangeOp::Delete, 0, "okay"),
                atspi_event(TextChangeOp::Insert, 0, "n"),
            ],
        )
        .await;
    }

    let promoted = std::fs::read_to_string(&config.substitutions_path).unwrap_or_default();
    assert_eq!(promoted, "");
    assert!(CorrectionStore::load(&config).unwrap().records().is_empty());
}

#[tokio::test]
async fn deleting_a_word_then_sending_records_only_the_word() {
    let dir = TempDir::new().unwrap();
    let config = make_config(dir.path(), 1, 3);

    let (pairs, _) = learn(
        &config,
        atspi_context("remove this extra word"),
        vec![
            atspi_event(TextChangeOp::Delete, 12, "extra "),
            atspi_event(TextChangeOp::Delete, 0, "remove this word"),
        ],
    )
    .await;

    assert_eq!(pairs, [pair("extra ", "")]);
}

#[tokio::test]
async fn fixing_a_word_then_sending_records_only_the_fix() {
    let dir = TempDir::new().unwrap();
    let config = make_config(dir.path(), 1, 3);

    let (pairs, _) = learn(
        &config,
        atspi_context("the cash is here"),
        vec![
            atspi_event(TextChangeOp::Delete, 4, "cash"),
            atspi_event(TextChangeOp::Insert, 4, "cache"),
            atspi_event(TextChangeOp::Delete, 0, "the cache is here"),
        ],
    )
    .await;

    assert_eq!(pairs, [pair("cash", "cache")]);
}

#[tokio::test]
async fn a_deletion_running_past_the_dictated_text_records_nothing() {
    let dir = TempDir::new().unwrap();
    let config = make_config(dir.path(), 1, 3);

    let (pairs, _) = learn(
        &config,
        atspi_context("see you at noon"),
        vec![
            atspi_event(TextChangeOp::Insert, 15, " ok bye"),
            atspi_event(TextChangeOp::Delete, 8, "at noon ok bye"),
            atspi_event(TextChangeOp::Delete, 4, "you"),
        ],
    )
    .await;

    assert_eq!(pairs, []);
}

#[tokio::test]
async fn a_fix_after_text_typed_in_front_is_still_a_correction() {
    let dir = TempDir::new().unwrap();
    let config = make_config(dir.path(), 1, 3);
    let (source, tx) = MockTextChangeSource::new(true);
    let store = CorrectionStore::empty(config.clone());
    let monitor = CorrectionMonitor::with_source(Arc::new(source), store, config);
    let handle = monitor.start_monitoring(atspi_context("the cash is here"));

    for event in [
        atspi_event(TextChangeOp::Insert, 0, "S"),
        atspi_event(TextChangeOp::Insert, 1, "o"),
        atspi_event(TextChangeOp::Insert, 2, " "),
        atspi_event(TextChangeOp::Delete, 7, "cash"),
        atspi_event(TextChangeOp::Insert, 7, "cache"),
    ] {
        tx.send(event).await.unwrap();
    }

    let corrections = handle.await.unwrap();
    assert_eq!(corrections.len(), 1);
    assert_eq!(corrections[0].original, "cash");
    assert_eq!(corrections[0].corrected, "cache");
    assert_eq!(corrections[0].context_before, "the ");
    assert_eq!(corrections[0].context_after, " is here");
}

#[tokio::test]
async fn a_source_that_resolves_the_dictated_text_reports_fixes_as_they_are() {
    let dir = TempDir::new().unwrap();
    let config = make_config(dir.path(), 1, 3);
    let (source, tx) = MockTextChangeSource::new(true);
    let source = source.with_scope(EventScope::DictatedText);
    let store = CorrectionStore::empty(config.clone());
    let monitor = CorrectionMonitor::with_source(Arc::new(source), store, config);
    let handle = monitor.start_monitoring(make_context("two words"));

    tx.send(make_event(TextChangeOp::Delete, 5, "words")).await.unwrap();
    tx.send(make_event(TextChangeOp::Insert, 5, "word5")).await.unwrap();

    let corrections = handle.await.unwrap();
    assert_eq!(corrections.len(), 1);
    assert_eq!(corrections[0].original, "words");
    assert_eq!(corrections[0].corrected, "word5");
}

#[tokio::test]
async fn sending_after_the_dictation_grew_and_shrank_records_no_deletion() {
    let dir = TempDir::new().unwrap();
    let config = make_config(dir.path(), 1, 3);

    let (pairs, _) = learn(
        &config,
        atspi_context("ok see you"),
        vec![
            atspi_event(TextChangeOp::Delete, 7, "you"),
            atspi_event(TextChangeOp::Insert, 7, "you tomorrow morning"),
            atspi_event(TextChangeOp::Delete, 19, " morning"),
            atspi_event(TextChangeOp::Delete, 0, "ok see you tomorrow"),
        ],
    )
    .await;

    assert_eq!(pairs, [pair("you", "you tomorrow morning")]);
}

#[tokio::test]
async fn an_unrelated_edit_far_away_does_not_split_a_fix() {
    let dir = TempDir::new().unwrap();
    let config = make_config(dir.path(), 1, 3);

    let (pairs, _) = learn(
        &config,
        atspi_context("the cash is here"),
        vec![
            atspi_event(TextChangeOp::Delete, 4, "cash"),
            atspi_event(TextChangeOp::Insert, 100, "x"),
            atspi_event(TextChangeOp::Insert, 4, "cache"),
        ],
    )
    .await;

    assert_eq!(pairs, [pair("cash", "cache")]);
}
