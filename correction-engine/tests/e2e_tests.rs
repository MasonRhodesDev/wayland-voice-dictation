use correction_engine::connection::{AtspiConnection, TextChangeSource};
use correction_engine::types::{EventSource, InjectionContext, TextChangeEvent, TextChangeOp};
use std::path::Path;
use std::time::Duration;
use tokio::time::{sleep, timeout};

const SUBSCRIBED_MARKER: &str = "/tmp/e2e-subscribed";
const READY_MARKER: &str = "/tmp/e2e-ready";
const TRIGGER_MARKER: &str = "/tmp/e2e-do-edit";
const APP_PID_FILE: &str = "/tmp/test-app.pid";

async fn wait_for_marker(path: &str, deadline: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        if Path::new(path).exists() {
            return true;
        }
        sleep(Duration::from_millis(100)).await;
    }
    Path::new(path).exists()
}

async fn walk_atspi_tree() {
    use atspi::object_ref::ObjectRefOwned;
    use atspi::proxy::accessible::{AccessibleProxy, ObjectRefExt};

    let conn = match atspi::AccessibilityConnection::new().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("walk_atspi_tree: connect failed: {e}");
            return;
        }
    };
    let zconn = conn.connection().clone();

    let root_res = async {
        AccessibleProxy::builder(&zconn)
            .destination("org.a11y.atspi.Registry")?
            .path("/org/a11y/atspi/accessible/root")?
            .cache_properties(atspi::zbus::proxy::CacheProperties::No)
            .build()
            .await
    }
    .await;
    let root = match root_res {
        Ok(p) => p,
        Err(e) => {
            eprintln!("walk_atspi_tree: root proxy failed: {e}");
            return;
        }
    };

    let mut queue: Vec<ObjectRefOwned> = match root.get_children().await {
        Ok(kids) => kids,
        Err(e) => {
            eprintln!("walk_atspi_tree: root has no children: {e}");
            return;
        }
    };
    let mut visited = 0usize;
    while let Some(obj) = queue.pop() {
        visited += 1;
        if visited > 500 {
            break;
        }
        if let Ok(proxy) = obj.as_accessible_proxy(&zconn).await {
            let _ = proxy.get_role_name().await;
            if let Ok(kids) = proxy.get_children().await {
                queue.extend(kids);
            }
        }
    }
    eprintln!("walk_atspi_tree: visited {visited} accessible node(s)");
}

#[tokio::test]
async fn atspi_receives_real_text_change_events() {
    if std::env::var("CORRECTION_E2E").as_deref() != Ok("1") {
        eprintln!("CORRECTION_E2E != 1 — skipping live AT-SPI2 e2e test (not in container)");
        return;
    }

    let _ = std::fs::remove_file(SUBSCRIBED_MARKER);
    let _ = std::fs::remove_file(TRIGGER_MARKER);

    let conn = AtspiConnection::connect().await;
    assert!(
        conn.is_available(),
        "AT-SPI2 bus reported unavailable inside the e2e container; \
         dbus/at-spi2-registryd wiring is broken"
    );
    let context = InjectionContext {
        text: String::new(),
        timestamp: chrono::Utc::now(),
        instant: std::time::Instant::now(),
        window_class: String::new(),
        window_pid: None,
        window_title: String::new(),
    };
    let mut rx = conn.subscribe(&context).await.expect("subscribe() failed");

    sleep(Duration::from_millis(500)).await;
    std::fs::write(SUBSCRIBED_MARKER, b"subscribed\n").expect("failed to write subscribed marker");

    assert!(
        wait_for_marker(READY_MARKER, Duration::from_secs(15)).await,
        "GTK4 test app never wrote {READY_MARKER}; app failed to start under weston"
    );

    sleep(Duration::from_millis(500)).await;
    walk_atspi_tree().await;

    std::fs::write(TRIGGER_MARKER, b"go\n").expect("failed to write trigger marker");

    let mut events: Vec<TextChangeEvent> = Vec::new();
    let mut saw_insert_cache = false;
    let mut saw_delete_cash = false;
    let mut cache_insert_source = None;

    let overall = timeout(Duration::from_secs(15), async {
        loop {
            match timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Some(ev)) => {
                    if ev.operation == TextChangeOp::Insert && ev.text.contains("cache") {
                        saw_insert_cache = true;
                        cache_insert_source = Some(ev.source.clone());
                    }
                    if ev.operation == TextChangeOp::Delete && ev.text.contains("cash") {
                        saw_delete_cash = true;
                    }
                    events.push(ev);

                    if saw_insert_cache {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_elapsed) => continue,
            }
        }
    })
    .await;

    let dump = || -> String {
        events
            .iter()
            .map(|e| {
                format!(
                    "  {:?} start={} len={} text={:?} app={:?}",
                    e.operation, e.start_pos, e.length, e.text, e.source
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    assert!(
        !events.is_empty(),
        "No AT-SPI2 text-change events received at all within 15s. \
         The real accessibility wiring is not emitting — this is the thing under test.\n\
         (ready marker seen, subscription live, tree walked, trigger written, \
         app should have edited)"
    );

    assert!(
        saw_insert_cache,
        "Received {} AT-SPI2 event(s) but none was an Insert containing \"cache\" \
         (the correction). overall_deadline_hit={}\nEvents:\n{}",
        events.len(),
        overall.is_err(),
        dump()
    );

    let app_pid: u32 = std::fs::read_to_string(APP_PID_FILE)
        .expect("GTK4 test app never wrote its pid")
        .trim()
        .parse()
        .expect("test app pid is not a number");
    assert_eq!(
        cache_insert_source,
        Some(EventSource::Process(app_pid)),
        "the correction must be attributed to the process that owns the edited window\nEvents:\n{}",
        dump()
    );

    eprintln!(
        "e2e OK: {} event(s); insert_cache={} delete_cash={}\n{}",
        events.len(),
        saw_insert_cache,
        saw_delete_cash,
        dump()
    );
}
