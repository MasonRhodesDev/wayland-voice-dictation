use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Stream, StreamConfig};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use super::{AudioBackend, AudioBackendConfig, AudioBackendFactory, DeviceInfo};
use crate::resample::Resampler;

struct Conversion {
    channels: usize,
    resampler: Resampler,
}

impl Conversion {
    fn new(channels: u16, in_rate: u32, out_rate: u32) -> Self {
        Self {
            channels: usize::from(channels.max(1)),
            resampler: Resampler::new(in_rate, out_rate),
        }
    }

    fn apply(&mut self, interleaved: &[f32]) -> Vec<i16> {
        let mono: Vec<i16> = interleaved
            .chunks(self.channels)
            .map(|frame| {
                let sample = frame.iter().sum::<f32>() / frame.len() as f32;
                (sample * 32767.0).clamp(-32768.0, 32767.0) as i16
            })
            .collect();
        self.resampler.push(&mono)
    }
}

fn capture_format(device: &cpal::Device, target_rate: u32) -> (StreamConfig, Conversion) {
    let requested = StreamConfig {
        channels: 1,
        sample_rate: cpal::SampleRate(target_rate),
        buffer_size: cpal::BufferSize::Default,
    };
    let supports_requested = device
        .supported_input_configs()
        .map(|mut configs| {
            configs.any(|c| {
                c.channels() == 1
                    && c.sample_format() == cpal::SampleFormat::F32
                    && (c.min_sample_rate().0..=c.max_sample_rate().0).contains(&target_rate)
            })
        })
        .unwrap_or(true);
    if supports_requested {
        return (requested, Conversion::new(1, target_rate, target_rate));
    }
    match device.default_input_config() {
        Ok(default) => {
            let native = StreamConfig {
                channels: default.channels(),
                sample_rate: default.sample_rate(),
                buffer_size: cpal::BufferSize::Default,
            };
            info!(
                "Device has no mono {} Hz input; capturing {} ch at {} Hz and converting",
                target_rate, native.channels, native.sample_rate.0
            );
            let conversion = Conversion::new(native.channels, native.sample_rate.0, target_rate);
            (native, conversion)
        }
        Err(e) => {
            warn!("Could not read the device's default input config: {}", e);
            (requested, Conversion::new(1, target_rate, target_rate))
        }
    }
}

pub struct CpalBackend {
    streams: Vec<Stream>,
    errored_streams: Arc<Mutex<HashSet<String>>>,
    last_audio_timestamp: Arc<AtomicU64>,
    samples_dropped: Arc<AtomicU64>,
}

impl CpalBackend {
    fn is_real_input_device(name: &str) -> bool {
        let name_lower = name.to_lowercase();
        if name_lower.contains(".monitor") || name_lower.contains("monitor") {
            return false;
        }
        if name_lower.contains("hdmi") {
            return false;
        }
        true
    }

    fn new(tx: mpsc::UnboundedSender<Vec<i16>>, config: &AudioBackendConfig) -> Result<Self> {
        let host = cpal::default_host();

        let device_name = config.device_name.as_deref();
        let device = match device_name {
            None | Some("default") => {
                info!("Using system default audio device (fast path)");
                let dev = host
                    .default_input_device()
                    .ok_or_else(|| anyhow::anyhow!("No default input device available"))?;
                if let Ok(name) = dev.name() {
                    info!("Default device: '{}'", name);
                }
                dev
            }
            Some("all") => {
                warn!("Multi-device 'all' mode is no longer supported, using default device");
                host.default_input_device()
                    .ok_or_else(|| anyhow::anyhow!("No default input device available"))?
            }
            Some(name) => {
                info!("Searching for device '{}'...", name);
                let mut found = None;
                if let Ok(devices) = host.input_devices() {
                    for device in devices {
                        if let Ok(device_name) = device.name() {
                            if device_name == name {
                                found = Some(device);
                                break;
                            }
                        }
                    }
                }
                match found {
                    Some(dev) => dev,
                    None => {
                        warn!("Device '{}' not found, using default", name);
                        host.default_input_device()
                            .ok_or_else(|| anyhow::anyhow!("No default input device available"))?
                    }
                }
            }
        };

        let (cb_tx, cb_rx) = crossbeam_channel::bounded::<Vec<i16>>(100);

        let (stream_config, mut conversion) = capture_format(&device, config.sample_rate);

        let errored_streams: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let last_audio_timestamp = Arc::new(AtomicU64::new(0));
        let samples_dropped = Arc::new(AtomicU64::new(0));

        let stream_id = device.name().unwrap_or_else(|_| "unknown".to_string());
        let threshold = config.silence_threshold;

        let error_stream_id = stream_id.clone();
        let errored_streams_clone = Arc::clone(&errored_streams);
        let samples_dropped_clone = Arc::clone(&samples_dropped);

        let stream = device
            .build_input_stream(
                &stream_config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let rms: f32 =
                        (data.iter().map(|&s| s * s).sum::<f32>() / data.len() as f32).sqrt();
                    if rms < threshold {
                        return;
                    }

                    let samples = conversion.apply(data);
                    if samples.is_empty() {
                        return;
                    }

                    if cb_tx.try_send(samples).is_err() {
                        samples_dropped_clone.fetch_add(1, Ordering::Relaxed);
                    }
                },
                move |err| {
                    if let Ok(mut errored) = errored_streams_clone.lock() {
                        if errored.insert(error_stream_id.clone()) {
                            error!(
                                "Audio stream '{}' error: {} (will retry on device reconnection)",
                                error_stream_id, err
                            );
                        }
                    }
                },
                None,
            )
            .map_err(|e| {
                anyhow::anyhow!("Failed to create audio stream for '{}': {}", stream_id, e)
            })?;
        stream.pause()?;

        info!("Created audio stream for: {}", stream_id);

        let tx_clone = tx;
        let last_ts_clone = Arc::clone(&last_audio_timestamp);
        let drops_clone = Arc::clone(&samples_dropped);
        std::thread::spawn(move || {
            let mut last_drop_log = 0u64;
            while let Ok(samples) = cb_rx.recv() {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                last_ts_clone.store(now_ms, Ordering::Relaxed);

                let current_drops = drops_clone.load(Ordering::Relaxed);
                if current_drops > last_drop_log && current_drops.is_multiple_of(100) {
                    warn!("Audio samples dropped: {} total", current_drops);
                    last_drop_log = current_drops;
                }

                if tx_clone.send(samples).is_err() {
                    break;
                }
            }
        });

        info!("CpalBackend initialized with single stream (direct channel)");
        Ok(Self { streams: vec![stream], errored_streams, last_audio_timestamp, samples_dropped })
    }

    pub fn last_audio_timestamp_ms(&self) -> u64 {
        self.last_audio_timestamp.load(Ordering::Relaxed)
    }

    pub fn samples_dropped_count(&self) -> u64 {
        self.samples_dropped.load(Ordering::Relaxed)
    }

    #[allow(dead_code)]
    pub fn has_healthy_streams(&self) -> bool {
        if let Ok(errored) = self.errored_streams.lock() {
            errored.len() < self.streams.len()
        } else {
            false
        }
    }
}

impl AudioBackend for CpalBackend {
    fn start(&self) -> Result<()> {
        for stream in &self.streams {
            stream.play()?;
        }
        info!("CpalBackend started ({} streams)", self.streams.len());
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        for stream in &self.streams {
            stream.pause()?;
        }
        info!("CpalBackend stopped ({} streams)", self.streams.len());
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        std::thread::sleep(std::time::Duration::from_millis(50));
        info!("CpalBackend flushed");
        Ok(())
    }

    fn releases_on_stop(&self) -> bool {
        true
    }
}

impl AudioBackendFactory for CpalBackend {
    fn create(
        tx: mpsc::UnboundedSender<Vec<i16>>,
        config: &AudioBackendConfig,
    ) -> Result<Box<dyn AudioBackend>> {
        Ok(Box::new(Self::new(tx, config)?))
    }

    fn list_devices() -> Result<Vec<DeviceInfo>> {
        let host = cpal::default_host();
        let default_name = host.default_input_device().and_then(|d| d.name().ok());

        let mut devices = Vec::new();
        if let Ok(input_devices) = host.input_devices() {
            for device in input_devices {
                if let Ok(name) = device.name() {
                    if Self::is_real_input_device(&name) {
                        let is_default = default_name.as_ref() == Some(&name);
                        devices.push(DeviceInfo { description: name.clone(), name, is_default });
                    }
                }
            }
        }

        Ok(devices)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_at_the_target_rate_passes_through_unchanged() {
        let mut conversion = Conversion::new(1, 16_000, 16_000);
        assert_eq!(
            conversion.apply(&[0.0, 0.5, -0.5, 1.0, -1.0]),
            vec![0, 16383, -16383, 32767, -32767]
        );
    }

    #[test]
    fn stereo_48k_becomes_mono_16k() {
        let frames = 4_800;
        let interleaved: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let left = (i % 100) as f32 / 100.0;
                [left, -left + 0.5]
            })
            .collect();
        let mut conversion = Conversion::new(2, 48_000, 16_000);
        let out: Vec<i16> = interleaved.chunks(960).flat_map(|c| conversion.apply(c)).collect();
        assert!((out.len() as i64 - 1_600).abs() <= 2, "got {}", out.len());
        assert!(out.iter().all(|&s| s == 8191), "each frame averages to 0.25");
    }
}
