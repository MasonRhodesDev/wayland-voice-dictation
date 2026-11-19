use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::control::{ControlContext, ControlReply, ControlRequest};
use crate::platform::unix_socket;

pub use unix_socket::ControlServer;

fn socket_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("VOICE_DICTATION_SOCKET") {
        return Ok(PathBuf::from(path));
    }
    let data_dir = dirs::data_dir().context("cannot locate ~/Library/Application Support")?;
    Ok(data_dir.join("voice-dictation").join("daemon.sock"))
}

pub async fn serve_control(ctx: ControlContext) -> Result<ControlServer> {
    unix_socket::serve(&socket_path()?, ctx).await
}

pub async fn send_request(req: ControlRequest) -> Result<ControlReply> {
    unix_socket::request(&socket_path()?, req).await
}

pub async fn daemon_running() -> bool {
    match socket_path() {
        Ok(path) => unix_socket::is_listening(&path).await,
        Err(_) => false,
    }
}

pub async fn daemon_exe() -> Option<PathBuf> {
    let pid = unix_socket::peer_pid(&socket_path().ok()?).await?;
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if len <= 0 {
        return None;
    }
    buf.truncate(len as usize);
    Some(PathBuf::from(OsString::from_vec(buf)))
}
