pub mod cpal_backend;

#[cfg(with_pipewire)]
pub mod pipewire_backend;

use anyhow::Result;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{info, warn};

#[derive(Clone)]
pub struct AudioBackendConfig {
    pub device_name: Option<String>,
    pub sample_rate: u32,
    pub silence_threshold: f32,
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    pub description: String,
    pub is_default: bool,
}

pub trait AudioBackend {
    fn start(&self) -> Result<()>;

    fn stop(&self) -> Result<()>;

    fn flush(&self) -> Result<()> {
        std::thread::sleep(std::time::Duration::from_millis(50));
        Ok(())
    }

    fn releases_on_stop(&self) -> bool;
}

pub trait AudioBackendFactory {
    fn create(
        tx: mpsc::UnboundedSender<Vec<i16>>,
        config: &AudioBackendConfig,
    ) -> Result<Box<dyn AudioBackend>>
    where
        Self: Sized;

    fn list_devices() -> Result<Vec<DeviceInfo>>
    where
        Self: Sized;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendType {
    #[default]
    Auto,
    Cpal,
    #[cfg(with_pipewire)]
    Pipewire,
}

impl BackendType {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "cpal" | "alsa" => Some(Self::Cpal),
            #[cfg(with_pipewire)]
            "pipewire" | "pw" => Some(Self::Pipewire),
            _ => None,
        }
    }
}

pub fn create_backend(
    backend_type: BackendType,
    tx: mpsc::UnboundedSender<Vec<i16>>,
    config: &AudioBackendConfig,
) -> Result<Box<dyn AudioBackend>> {
    match backend_type {
        BackendType::Auto => create_backend_auto(tx, config),
        BackendType::Cpal => {
            info!("Using cpal audio backend");
            cpal_backend::CpalBackend::create(tx, config)
        }
        #[cfg(with_pipewire)]
        BackendType::Pipewire => {
            info!("Using PipeWire audio backend");
            pipewire_backend::PipewireBackend::create(tx, config)
        }
    }
}

fn create_backend_auto(
    tx: mpsc::UnboundedSender<Vec<i16>>,
    config: &AudioBackendConfig,
) -> Result<Box<dyn AudioBackend>> {
    #[cfg(with_pipewire)]
    {
        if pipewire_backend::PipewireBackend::is_available() {
            info!("PipeWire availability check passed, attempting to create backend...");
            match pipewire_backend::PipewireBackend::create(tx.clone(), config) {
                Ok(backend) => {
                    info!("Using PipeWire audio backend (auto-detected)");
                    info!("  Supports concurrent mic access (no browser conflicts)");
                    return Ok(backend);
                }
                Err(e) => {
                    warn!("PipeWire backend creation failed: {e}");
                    warn!("  Falling back to cpal/ALSA (will hold exclusive mic access)");
                    warn!("  Check that the PipeWire user service is running");
                }
            }
        } else {
            warn!("PipeWire not available on system");
            warn!("  Check that the PipeWire user service is running");
            warn!("  Using cpal/ALSA backend (will hold exclusive mic access)");
        }
    }

    #[cfg(pipewire_disabled)]
    {
        warn!("PipeWire feature not enabled at compile time");
        warn!("  Rebuild with: cargo build --features pipewire");
        warn!("  Using cpal/ALSA backend (will hold exclusive mic access)");
    }

    info!("Using cpal/ALSA audio backend");
    info!("  Will release mic after idle timeout (default: 30s)");
    cpal_backend::CpalBackend::create(tx, config)
}

#[derive(Debug, Deserialize)]
struct PactlSource {
    name: String,
    description: String,
    monitor_source: String,
}

pub fn list_devices(backend_type: BackendType) -> Result<Vec<DeviceInfo>> {
    match list_devices_pactl() {
        Ok(devices) if !devices.is_empty() => {
            info!("Got {} devices from pactl", devices.len());
            Ok(devices)
        }
        Ok(_) => {
            warn!("pactl returned no devices, falling back to backend enumeration");
            list_devices_backend(backend_type)
        }
        Err(e) => {
            warn!("pactl failed ({}), falling back to backend enumeration", e);
            list_devices_backend(backend_type)
        }
    }
}

fn list_devices_pactl() -> Result<Vec<DeviceInfo>> {
    let output =
        std::process::Command::new("pactl").args(["--format=json", "list", "sources"]).output()?;

    if !output.status.success() {
        anyhow::bail!("pactl exited with status {}", output.status);
    }

    let sources: Vec<PactlSource> = serde_json::from_slice(&output.stdout)?;

    let default_source =
        std::process::Command::new("pactl").args(["get-default-source"]).output().ok().and_then(
            |o| {
                if o.status.success() {
                    String::from_utf8(o.stdout).ok().map(|s| s.trim().to_string())
                } else {
                    None
                }
            },
        );

    let devices: Vec<DeviceInfo> = sources
        .into_iter()
        .filter(|s| s.monitor_source.is_empty())
        .map(|s| {
            let is_default = default_source.as_deref() == Some(&s.name);
            DeviceInfo { name: s.name, description: s.description, is_default }
        })
        .collect();

    Ok(devices)
}

pub fn list_devices_backend(backend_type: BackendType) -> Result<Vec<DeviceInfo>> {
    match backend_type {
        BackendType::Auto | BackendType::Cpal => cpal_backend::CpalBackend::list_devices(),
        #[cfg(with_pipewire)]
        BackendType::Pipewire => pipewire_backend::PipewireBackend::list_devices(),
    }
}
