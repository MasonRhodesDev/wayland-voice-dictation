use anyhow::Result;
use std::path::PathBuf;
use tracing::info;
use zbus::interface;

use crate::control::{handle, ControlContext, ControlReply, ControlRequest};

const SERVICE_NAME: &str = "com.voicedictation.Daemon";
const OBJECT_PATH: &str = "/com/voicedictation/Control";
const INTERFACE_NAME: &str = "com.voicedictation.Control";

pub struct ControlServer {
    _connection: zbus::Connection,
}

struct VoiceDictationService {
    ctx: ControlContext,
}

impl VoiceDictationService {
    async fn unit(&self, req: ControlRequest) -> zbus::fdo::Result<()> {
        match handle(req, &self.ctx).await {
            ControlReply::Error(message) => Err(zbus::fdo::Error::Failed(message)),
            _ => Ok(()),
        }
    }
}

fn unexpected(reply: ControlReply) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(format!("unexpected reply: {:?}", reply))
}

#[interface(name = "com.voicedictation.Control")]
impl VoiceDictationService {
    async fn start_recording(&self) -> zbus::fdo::Result<()> {
        self.unit(ControlRequest::StartRecording).await
    }

    async fn stop_recording(&self) -> zbus::fdo::Result<()> {
        self.unit(ControlRequest::StopRecording).await
    }

    async fn confirm(&self) -> zbus::fdo::Result<()> {
        self.unit(ControlRequest::Confirm).await
    }

    async fn status(&self) -> zbus::fdo::Result<(String, bool)> {
        match handle(ControlRequest::Status, &self.ctx).await {
            ControlReply::Status { state, session_active } => {
                Ok((state.to_string(), session_active))
            }
            other => Err(unexpected(other)),
        }
    }

    async fn health_check(&self) -> zbus::fdo::Result<(String, String, String)> {
        match handle(ControlRequest::HealthCheck, &self.ctx).await {
            ControlReply::Health { gui, engine, audio } => Ok((gui, engine, audio)),
            other => Err(unexpected(other)),
        }
    }

    async fn snapshot_correction(&self) -> zbus::fdo::Result<()> {
        self.unit(ControlRequest::SnapshotCorrection).await
    }

    async fn shutdown(&self) -> zbus::fdo::Result<()> {
        self.unit(ControlRequest::Shutdown).await
    }
}

pub async fn serve_control(ctx: ControlContext) -> Result<ControlServer> {
    let connection = zbus::connection::Builder::session()?
        .name(SERVICE_NAME)?
        .serve_at(OBJECT_PATH, VoiceDictationService { ctx })?
        .build()
        .await?;

    info!("D-Bus service registered at {}", SERVICE_NAME);

    Ok(ControlServer { _connection: connection })
}

fn method_name(req: ControlRequest) -> &'static str {
    match req {
        ControlRequest::StartRecording => "StartRecording",
        ControlRequest::StopRecording => "StopRecording",
        ControlRequest::Confirm => "Confirm",
        ControlRequest::Status => "Status",
        ControlRequest::HealthCheck => "HealthCheck",
        ControlRequest::SnapshotCorrection => "SnapshotCorrection",
        ControlRequest::Shutdown => "Shutdown",
    }
}

async fn proxy(connection: &zbus::Connection) -> zbus::Result<zbus::Proxy<'_>> {
    zbus::Proxy::new(connection, SERVICE_NAME, OBJECT_PATH, INTERFACE_NAME).await
}

pub async fn send_request(req: ControlRequest) -> Result<ControlReply> {
    request_on(&zbus::Connection::session().await?, req).await
}

async fn request_on(connection: &zbus::Connection, req: ControlRequest) -> Result<ControlReply> {
    let proxy = proxy(connection).await?;
    let reply = match proxy.call_method(method_name(req), &()).await {
        Ok(reply) => reply,
        Err(zbus::Error::MethodError(_, Some(detail), _)) => {
            return Ok(ControlReply::Error(detail));
        }
        Err(e) => return Err(e.into()),
    };
    Ok(match req {
        ControlRequest::Status => {
            let (state, session_active): (String, bool) = reply.body().deserialize()?;
            ControlReply::Status { state: state.parse()?, session_active }
        }
        ControlRequest::HealthCheck => {
            let (gui, engine, audio) = reply.body().deserialize()?;
            ControlReply::Health { gui, engine, audio }
        }
        _ => ControlReply::Ok,
    })
}

pub async fn daemon_running() -> bool {
    let Ok(connection) = zbus::Connection::session().await else {
        return false;
    };
    match proxy(&connection).await {
        Ok(proxy) => proxy.introspect().await.is_ok(),
        Err(_) => false,
    }
}

pub async fn daemon_exe() -> Option<PathBuf> {
    let connection = zbus::Connection::session().await.ok()?;
    let proxy = zbus::Proxy::new(
        &connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await
    .ok()?;
    let pid: u32 = proxy.call("GetConnectionUnixProcessID", &(SERVICE_NAME)).await.ok()?;
    std::fs::read_link(format!("/proc/{}/exe", pid)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::tests::context;
    use crate::control::{DaemonCommand, DaemonState};
    use crate::platform::Permission;
    use tokio::net::UnixStream;

    async fn connected(ctx: ControlContext) -> (zbus::Connection, zbus::Connection) {
        let (server_end, client_end) = UnixStream::pair().unwrap();
        let server = async {
            zbus::connection::Builder::unix_stream(server_end)
                .server(zbus::Guid::generate())
                .unwrap()
                .p2p()
                .serve_at(OBJECT_PATH, VoiceDictationService { ctx })
                .unwrap()
                .build()
                .await
                .unwrap()
        };
        let client = async {
            zbus::connection::Builder::unix_stream(client_end).p2p().build().await.unwrap()
        };
        tokio::join!(server, client)
    }

    fn microphone_denied() -> Vec<Permission> {
        vec![Permission { name: "Microphone", remedy: "allow it".into() }]
    }

    #[tokio::test]
    async fn dbus_round_trip_keeps_the_wire_contract() {
        let (ctx, mut commands, state) = context(Vec::new);
        let (_server, client) = connected(ctx).await;

        assert_eq!(request_on(&client, ControlRequest::Confirm).await.unwrap(), ControlReply::Ok);
        assert!(matches!(commands.try_recv(), Ok(DaemonCommand::Confirm)));

        state.send(DaemonState::Processing).unwrap();
        assert_eq!(
            request_on(&client, ControlRequest::Status).await.unwrap(),
            ControlReply::Status { state: DaemonState::Processing, session_active: true }
        );

        let proxy = proxy(&client).await.unwrap();
        let raw: (String, String, String) = proxy.call("HealthCheck", &()).await.unwrap();
        assert_eq!(raw, ("unavailable".into(), "not loaded".into(), "idle".into()));
    }

    #[tokio::test]
    async fn dbus_failure_comes_back_as_an_error_reply() {
        let (ctx, _commands, _state) = context(microphone_denied);
        let (_server, client) = connected(ctx).await;
        let ControlReply::Error(message) =
            request_on(&client, ControlRequest::StartRecording).await.unwrap()
        else {
            panic!("expected an error reply");
        };
        assert!(message.contains("Microphone"), "{message}");
    }
}
