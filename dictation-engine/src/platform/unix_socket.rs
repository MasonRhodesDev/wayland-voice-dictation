use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::control::{handle, ControlContext, ControlReply, ControlRequest};

pub struct ControlServer {
    path: PathBuf,
    accept_loop: JoinHandle<()>,
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.accept_loop.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

pub async fn serve(path: &Path, ctx: ControlContext) -> Result<ControlServer> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    if is_listening(path).await {
        anyhow::bail!("another daemon is already serving {}", path.display());
    }
    let _ = std::fs::remove_file(path);
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    info!("Control socket listening at {}", path.display());

    let ctx = Arc::new(ctx);
    let accept_loop = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let ctx = Arc::clone(&ctx);
                    tokio::spawn(async move {
                        if let Err(e) = serve_connection(stream, &ctx).await {
                            debug!("Control connection ended: {}", e);
                        }
                    });
                }
                Err(e) => {
                    warn!("Control socket accept failed: {}", e);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });
    Ok(ControlServer { path: path.to_path_buf(), accept_loop })
}

async fn serve_connection(stream: UnixStream, ctx: &ControlContext) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    if BufReader::new(read).read_line(&mut line).await? == 0 {
        return Ok(());
    }
    let reply = match serde_json::from_str::<ControlRequest>(&line) {
        Ok(req) => handle(req, ctx).await,
        Err(e) => ControlReply::Error(format!("invalid request: {}", e)),
    };
    write_line(&mut write, &reply).await
}

async fn write_line(writer: &mut (impl AsyncWrite + Unpin), value: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    Ok(())
}

pub async fn request(path: &Path, req: ControlRequest) -> Result<ControlReply> {
    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("connecting to {}", path.display()))?;
    let (read, mut write) = stream.into_split();
    write_line(&mut write, &req).await?;
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    Ok(serde_json::from_str(&line)?)
}

pub async fn is_listening(path: &Path) -> bool {
    UnixStream::connect(path).await.is_ok()
}

pub async fn peer_pid(path: &Path) -> Option<i32> {
    let stream = UnixStream::connect(path).await.ok()?;
    stream.peer_cred().ok()?.pid()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::tests::context;
    use crate::control::{DaemonCommand, DaemonState};

    #[tokio::test]
    async fn requests_round_trip_over_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("daemon.sock");
        let (ctx, mut commands, state) = context(Vec::new);
        let _server = serve(&path, ctx).await.unwrap();

        assert!(is_listening(&path).await);
        assert_eq!(request(&path, ControlRequest::Confirm).await.unwrap(), ControlReply::Ok);
        assert!(matches!(commands.recv().await, Some(DaemonCommand::Confirm)));

        state.send(DaemonState::Recording).unwrap();
        assert_eq!(
            request(&path, ControlRequest::Status).await.unwrap(),
            ControlReply::Status { state: DaemonState::Recording, session_active: true }
        );
        assert_eq!(peer_pid(&path).await, Some(std::process::id() as i32));
    }

    #[tokio::test]
    async fn a_stale_socket_is_replaced_and_a_live_one_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists() && !is_listening(&path).await);

        let (ctx, _commands, _state) = context(Vec::new);
        let server = serve(&path, ctx).await.unwrap();
        let (second, _commands, _state) = context(Vec::new);
        assert!(serve(&path, second).await.is_err(), "a second daemon must not steal the socket");

        drop(server);
        assert!(!path.exists(), "the socket is removed on shutdown");
    }

    #[tokio::test]
    async fn garbage_gets_an_error_reply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.sock");
        let (ctx, _commands, _state) = context(Vec::new);
        let _server = serve(&path, ctx).await.unwrap();

        let stream = UnixStream::connect(&path).await.unwrap();
        let (read, mut write) = stream.into_split();
        write.write_all(b"not json\n").await.unwrap();
        let mut line = String::new();
        BufReader::new(read).read_line(&mut line).await.unwrap();
        assert!(matches!(serde_json::from_str(&line).unwrap(), ControlReply::Error(_)));
    }
}
