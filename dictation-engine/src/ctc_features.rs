use ndarray::Array2;
use rustfft::{num_complex::Complex, FftPlanner};
use std::f32::consts::PI;

const SAMPLE_RATE: usize = 16000;
const FEATURE_SIZE: usize = 80;
const N_FFT: usize = 512;
const WIN_LENGTH: usize = 400;
const HOP_LENGTH: usize = 160;
const PREEMPHASIS: f32 = 0.97;

pub fn extract_features(samples: &[i16], sample_rate: u32) -> anyhow::Result<Array2<f32>> {
    if sample_rate != SAMPLE_RATE as u32 {
        anyhow::bail!("ctc_features requires 16 kHz audio, got {} Hz", sample_rate);
    }

    if samples.is_empty() {
        return Ok(Array2::<f32>::zeros((0, FEATURE_SIZE)));
    }

    let mut audio: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();

    audio = apply_preemphasis(&audio, PREEMPHASIS);

    let power_spec = stft(&audio, N_FFT, HOP_LENGTH, WIN_LENGTH);

    let filterbank = create_mel_filterbank(N_FFT, FEATURE_SIZE, SAMPLE_RATE);
    let mel_spec = filterbank.dot(&power_spec);

    let mel_spec = mel_spec.mapv(|x| x.max(1e-10_f32).ln());

    let mut mel_spec = mel_spec.t().to_owned();

    let num_frames = mel_spec.shape()[0];
    let num_features = mel_spec.shape()[1];

    for feat_idx in 0..num_features {
        let mut col = mel_spec.column_mut(feat_idx);
        let mean: f32 = col.iter().sum::<f32>() / num_frames as f32;
        let variance: f32 =
            col.iter().map(|&x| (x - mean).powi(2)).sum::<f32>() / num_frames as f32;
        let std = variance.sqrt().max(1e-10);
        for val in col.iter_mut() {
            *val = (*val - mean) / std;
        }
    }

    Ok(mel_spec)
}

fn apply_preemphasis(audio: &[f32], coef: f32) -> Vec<f32> {
    if audio.is_empty() {
        return Vec::new();
    }
    let mut result = Vec::with_capacity(audio.len());
    result.push(audio[0]);
    for i in 1..audio.len() {
        result.push(audio[i] - coef * audio[i - 1]);
    }
    result
}

fn hann_window(window_length: usize) -> Vec<f32> {
    (0..window_length)
        .map(|i| 0.5 - 0.5 * ((2.0 * PI * i as f32) / (window_length as f32 - 1.0)).cos())
        .collect()
}

fn stft(audio: &[f32], n_fft: usize, hop_length: usize, win_length: usize) -> Array2<f32> {
    let window = hann_window(win_length);
    if audio.len() < win_length {
        let freq_bins = n_fft / 2 + 1;
        return Array2::<f32>::zeros((freq_bins, 0));
    }
    let num_frames = (audio.len() - win_length) / hop_length + 1;
    let freq_bins = n_fft / 2 + 1;
    let mut spectrogram = Array2::<f32>::zeros((freq_bins, num_frames));

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(n_fft);

    for frame_idx in 0..num_frames {
        let start = frame_idx * hop_length;

        let mut frame: Vec<Complex<f32>> = vec![Complex::new(0.0, 0.0); n_fft];
        for i in 0..win_length.min(audio.len() - start) {
            frame[i] = Complex::new(audio[start + i] * window[i], 0.0);
        }

        fft.process(&mut frame);

        for k in 0..freq_bins {
            let magnitude = frame[k].norm();
            spectrogram[[k, frame_idx]] = magnitude * magnitude;
        }
    }

    spectrogram
}

fn hz_to_mel(freq: f32) -> f32 {
    2595.0 * (1.0 + freq / 700.0).log10()
}

fn mel_to_hz(mel: f32) -> f32 {
    700.0 * (10.0_f32.powf(mel / 2595.0) - 1.0)
}

fn create_mel_filterbank(n_fft: usize, n_mels: usize, sample_rate: usize) -> Array2<f32> {
    let freq_bins = n_fft / 2 + 1;
    let mut filterbank = Array2::<f32>::zeros((n_mels, freq_bins));

    let min_mel = hz_to_mel(0.0);
    let max_mel = hz_to_mel(sample_rate as f32 / 2.0);

    let mel_points: Vec<f32> = (0..=n_mels + 1)
        .map(|i| mel_to_hz(min_mel + (max_mel - min_mel) * i as f32 / (n_mels + 1) as f32))
        .collect();

    let freq_bin_width = sample_rate as f32 / n_fft as f32;

    for mel_idx in 0..n_mels {
        let left = mel_points[mel_idx];
        let center = mel_points[mel_idx + 1];
        let right = mel_points[mel_idx + 2];

        for freq_idx in 0..freq_bins {
            let freq = freq_idx as f32 * freq_bin_width;

            if freq >= left && freq <= center {
                filterbank[[mel_idx, freq_idx]] = (freq - left) / (center - left);
            } else if freq > center && freq <= right {
                filterbank[[mel_idx, freq_idx]] = (right - freq) / (right - center);
            }
        }
    }

    filterbank
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_wave_1s() -> Vec<i16> {
        let freq = 440.0_f32;
        let sr = 16000_f32;
        (0..16000)
            .map(|i| {
                let t = i as f32 / sr;
                (0.5 * (2.0 * PI * freq * t).sin() * i16::MAX as f32) as i16
            })
            .collect()
    }

    #[test]
    fn test_output_shape() {
        let samples = sine_wave_1s();
        let features = extract_features(&samples, 16000).expect("extract_features failed");
        let shape = features.shape();
        assert_eq!(shape[0], 98, "unexpected number of frames");
        assert_eq!(shape[1], 80, "unexpected number of mel bins");
    }

    #[test]
    fn test_output_is_finite() {
        let samples = sine_wave_1s();
        let features = extract_features(&samples, 16000).expect("extract_features failed");
        for &val in features.iter() {
            assert!(val.is_finite(), "non-finite value in features: {val}");
        }
    }

    #[test]
    fn test_normalized_values_reasonable_range() {
        let samples = sine_wave_1s();
        let features = extract_features(&samples, 16000).expect("extract_features failed");
        let mut out_of_range = 0usize;
        for &val in features.iter() {
            if val.abs() > 10.0 {
                out_of_range += 1;
            }
        }
        let total = features.len();
        assert!(
            out_of_range * 100 < total,
            "too many out-of-range values after normalization: {out_of_range}/{total}"
        );
    }

    #[test]
    fn test_wrong_sample_rate_errors() {
        let samples = sine_wave_1s();
        let result = extract_features(&samples, 8000);
        assert!(result.is_err(), "expected error for wrong sample rate");
    }

    #[test]
    fn test_empty_samples() {
        let features = extract_features(&[], 16000).expect("extract_features failed on empty");
        assert_eq!(features.shape()[0], 0);
        assert_eq!(features.shape()[1], 80);
    }
}
