use anyhow::Result;
use tracing::info;

pub struct IdleInhibitor {
    _awake: DropOffRuntime<keepawake::KeepAwake>,
}

pub async fn inhibit_idle(reason: &str) -> Result<IdleInhibitor> {
    let reason = reason.to_string();
    let awake = tokio::task::spawn_blocking(move || {
        keepawake::Builder::default()
            .idle(true)
            .sleep(cfg!(target_os = "linux"))
            .reason(reason)
            .app_name("voice-dictation")
            .create()
    })
    .await??;
    info!("Idle inhibit acquired");
    Ok(IdleInhibitor { _awake: DropOffRuntime(Some(awake)) })
}

struct DropOffRuntime<T: Send + 'static>(Option<T>);

impl<T: Send + 'static> Drop for DropOffRuntime<T> {
    fn drop(&mut self) {
        if let Some(value) = self.0.take() {
            std::thread::spawn(move || drop(value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    struct BlocksOnDrop(mpsc::Sender<()>);

    impl Drop for BlocksOnDrop {
        fn drop(&mut self) {
            tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {});
            let _ = self.0.send(());
        }
    }

    #[tokio::test]
    async fn dropping_inside_the_runtime_does_not_panic() {
        let (tx, rx) = mpsc::channel();
        drop(DropOffRuntime(Some(BlocksOnDrop(tx))));
        rx.recv_timeout(Duration::from_secs(5)).expect("value was dropped off the runtime");
    }
}
