use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::platform::applescript::{parse_reply, Reply};

const OSASCRIPT_TIMEOUT: Duration = Duration::from_secs(5);

struct Player {
    name: &'static str,
    bundle_id: &'static str,
    automation_warned: AtomicBool,
}

static PLAYERS: [Player; 2] = [
    Player {
        name: "Music",
        bundle_id: "com.apple.Music",
        automation_warned: AtomicBool::new(false),
    },
    Player {
        name: "Spotify",
        bundle_id: "com.spotify.client",
        automation_warned: AtomicBool::new(false),
    },
];

pub struct PausedMedia(Vec<&'static Player>);

pub fn pause_media_if_playing() -> Option<PausedMedia> {
    let paused: Vec<&'static Player> =
        PLAYERS.iter().filter(|player| player.is_running() && player.pause_if_playing()).collect();
    (!paused.is_empty()).then_some(PausedMedia(paused))
}

pub fn resume_media(paused: PausedMedia) {
    for player in paused.0 {
        let script = format!(
            "if application id \"{id}\" is running then\ntell application id \"{id}\" to play\nend if",
            id = player.bundle_id
        );
        if player.run(&script).is_some() {
            info!("Resumed {}", player.name);
        }
    }
}

impl Player {
    fn is_running(&self) -> bool {
        let script = format!("application id \"{}\" is running", self.bundle_id);
        self.run(&script).as_deref() == Some("true")
    }

    fn pause_if_playing(&self) -> bool {
        let script = format!(
            "tell application id \"{}\"\nif player state is playing then\npause\nreturn \"paused\"\nend if\nend tell\nreturn \"idle\"",
            self.bundle_id
        );
        let paused = self.run(&script).as_deref() == Some("paused");
        if paused {
            info!("Paused {}", self.name);
        }
        paused
    }

    fn run(&self, script: &str) -> Option<String> {
        let output = osascript(script)?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        match parse_reply(output.status.success(), &stdout, &stderr) {
            Reply::Value(value) => Some(value.to_string()),
            Reply::NotAuthorized => {
                if !self.automation_warned.swap(true, Ordering::Relaxed) {
                    warn!(
                        "Cannot pause or resume {}: {}",
                        self.name,
                        super::permissions::automation()
                    );
                }
                None
            }
            Reply::Failed(message) => {
                debug!("osascript for {} failed: {}", self.name, message);
                None
            }
        }
    }
}

fn osascript(script: &str) -> Option<Output> {
    let mut child = Command::new("osascript")
        .args(["-e", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| debug!("Failed to run osascript: {}", e))
        .ok()?;
    let deadline = Instant::now() + OSASCRIPT_TIMEOUT;
    while child.try_wait().ok()?.is_none() {
        if Instant::now() >= deadline {
            debug!("osascript timed out after {:?}", OSASCRIPT_TIMEOUT);
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().ok()
}
