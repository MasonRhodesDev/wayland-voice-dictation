use crate::types::{TextChangeEvent, TextChangeOp};
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

#[allow(clippy::double_must_use)]
#[async_trait]
pub trait TextChangeSource: Send + Sync {
    async fn subscribe(&self) -> Result<mpsc::Receiver<TextChangeEvent>>;

    fn is_available(&self) -> bool;
}

pub struct AtspiConnection {
    available: bool,
}

impl AtspiConnection {
    pub async fn connect() -> Self {
        match Self::try_connect().await {
            Ok(()) => {
                info!("AT-SPI2 accessibility bus connected");
                Self { available: true }
            }
            Err(e) => {
                warn!(
                    "AT-SPI2 accessibility bus not available: {} — correction detection will be disabled",
                    e
                );
                Self { available: false }
            }
        }
    }

    async fn try_connect() -> Result<()> {
        let _conn = atspi::AccessibilityConnection::new().await?;
        Ok(())
    }
}

#[async_trait]
impl TextChangeSource for AtspiConnection {
    async fn subscribe(&self) -> Result<mpsc::Receiver<TextChangeEvent>> {
        let (tx, rx) = mpsc::channel(256);

        if !self.available {
            return Ok(rx);
        }

        tokio::spawn(async move {
            if let Err(e) = run_event_listener(tx).await {
                error!("AT-SPI2 event listener error: {}", e);
            }
        });

        Ok(rx)
    }

    fn is_available(&self) -> bool {
        self.available
    }
}

async fn run_event_listener(tx: mpsc::Sender<TextChangeEvent>) -> Result<()> {
    use atspi::events::object::TextChangedEvent;
    use futures_util::StreamExt;

    let atspi = atspi::AccessibilityConnection::new().await?;

    atspi.register_event::<TextChangedEvent>().await?;
    debug!("Registered for AT-SPI2 TextChangedEvent");

    let events = atspi.event_stream();
    tokio::pin!(events);

    while let Some(item) = events.next().await {
        let ev = match item {
            Ok(ev) => ev,
            Err(e) => {
                debug!("Skipping non-event AT-SPI2 stream item: {}", e);
                continue;
            }
        };

        let Ok(text_event) = TextChangedEvent::try_from(ev) else {
            continue;
        };

        let operation = match text_event.operation {
            atspi::Operation::Insert => TextChangeOp::Insert,
            atspi::Operation::Delete => TextChangeOp::Delete,
        };

        let source_app = text_event.item.name_as_str().unwrap_or("").to_string();

        let event = TextChangeEvent {
            operation,
            start_pos: text_event.start_pos,
            length: text_event.length,
            text: text_event.text,
            timestamp: std::time::Instant::now(),
            source_app,
        };

        if tx.send(event).await.is_err() {
            debug!("Event channel closed, stopping AT-SPI2 listener");
            break;
        }
    }

    Ok(())
}

pub struct MockTextChangeSource {
    tx: mpsc::Sender<TextChangeEvent>,
    rx: tokio::sync::Mutex<Option<mpsc::Receiver<TextChangeEvent>>>,
    available: bool,
}

impl MockTextChangeSource {
    pub fn new(available: bool) -> (Self, mpsc::Sender<TextChangeEvent>) {
        let (tx, rx) = mpsc::channel(256);
        let source = Self { tx: tx.clone(), rx: tokio::sync::Mutex::new(Some(rx)), available };
        (source, tx)
    }

    pub fn sender(&self) -> mpsc::Sender<TextChangeEvent> {
        self.tx.clone()
    }
}

#[async_trait]
impl TextChangeSource for MockTextChangeSource {
    async fn subscribe(&self) -> Result<mpsc::Receiver<TextChangeEvent>> {
        let mut rx_guard = self.rx.lock().await;
        rx_guard.take().ok_or_else(|| anyhow::anyhow!("MockTextChangeSource already subscribed"))
    }

    fn is_available(&self) -> bool {
        self.available
    }
}
