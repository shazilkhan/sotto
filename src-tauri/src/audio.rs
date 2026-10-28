//! Mic capture. The stream exists only while the hotkey is held: no open mic
//! while idle means no CPU use, no privacy indicator, and no Bluetooth headset
//! stuck in low-quality call mode.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

pub const TARGET_RATE: u32 = 16_000;

/// Loudest sample since the last read, as f32 bits (non-negative floats
/// order the same as their bits, so `fetch_max` works). The pill and the
/// Settings level meter each swap their own copy back to zero.
pub static PILL_LEVEL: AtomicU32 = AtomicU32::new(0);
pub static UI_LEVEL: AtomicU32 = AtomicU32::new(0);

pub struct Recorder {
    stream: cpal::Stream,
    buf: Arc<Mutex<Vec<f32>>>,
    rate: u32,
}

fn device_name(d: &cpal::Device) -> Option<String> {
    d.description().ok().map(|desc| desc.name().to_string())
}

pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    let Ok(devices) = host.input_devices() else { return vec![] };
    devices.filter_map(|d| device_name(&d)).collect()
}

fn pick_device(name: Option<&str>) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if let Some(name) = name {
        let found = host
            .input_devices()
            .ok()
            .and_then(|mut it| it.find(|d| device_name(d).as_deref() == Some(name)));
        if found.is_some() {
            return found;
        }
        log::warn!("mic {name:?} not found, using the default");
    }
    host.default_input_device()
}

impl Recorder {
    pub fn start(mic: Option<&str>) -> Result<Self, String> {
        let device = pick_device(mic).ok_or("no microphone found")?;
        let supported = device.default_input_config().map_err(|e| e.to_string())?;
        let rate = supported.sample_rate();
        let channels = supported.channels() as usize;
        let config = supported.config();
        let buf = Arc::new(Mutex::new(Vec::with_capacity(rate as usize * 20)));

        let stream = match supported.sample_format() {
            SampleFormat::F32 => build::<f32>(&device, config, channels, &buf),
            SampleFormat::I16 => build::<i16>(&device, config, channels, &buf),
            SampleFormat::I32 => build::<i32>(&device, config, channels, &buf),
            SampleFormat::U16 => build::<u16>(&device, config, channels, &buf),
            SampleFormat::U8 => build::<u8>(&device, config, channels, &buf),
            SampleFormat::F64 => build::<f64>(&device, config, channels, &buf),
            other => return Err(format!("unsupported sample format {other:?}")),
        }?;
        stream.play().map_err(|e| e.to_string())?;
        log::info!(
            "recording from {:?} at {rate} Hz, {channels} ch",
            device_name(&device).unwrap_or_default()
        );
        Ok(Self { stream, buf, rate })
    }

    /// Stops the stream and returns mono samples at the device rate.
    pub fn stop(self) -> (Vec<f32>, u32) {
        drop(self.stream);
        let samples = std::mem::take(&mut *self.buf.lock().unwrap());
        (samples, self.rate)
    }
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    buf: &Arc<Mutex<Vec<f32>>>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let buf = buf.clone();
    device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _| {
                let mut b = buf.lock().unwrap();
                let mut peak = 0f32;
                for frame in data.chunks_exact(channels) {
                    let sum: f32 = frame.iter().map(|&s| s.to_sample::<f32>()).sum();
                    let s = sum / channels as f32;
                    peak = peak.max(s.abs());
                    b.push(s);
                }
                PILL_LEVEL.fetch_max(peak.to_bits(), Ordering::Relaxed);
                UI_LEVEL.fetch_max(peak.to_bits(), Ordering::Relaxed);
            },
            |e| log::warn!("mic stream error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())
}

/// Band-limited resampling (windowed sinc, polyphase table). Runs once per
/// dictation on the whole buffer: a few ms per 10 s of audio.
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    const PHASES: usize = 256;
    let ratio = to as f64 / from as f64;
    // Low-pass a little under the output Nyquist to keep aliasing out.
    let cutoff = ratio.min(1.0) * 0.92;
    let half = 12.0 / cutoff;
    let h = half.ceil() as usize;
    let taps = 2 * h;

    // table[p * taps + j] weights input[base - h + 1 + j] for fractional
    // offset p / PHASES; each phase is normalised to unity DC gain.
    let mut table = vec![0f32; (PHASES + 1) * taps];
    for p in 0..=PHASES {
        let frac = p as f64 / PHASES as f64;
        let row = &mut table[p * taps..(p + 1) * taps];
        let mut sum = 0.0;
        for (j, w) in row.iter_mut().enumerate() {
            let d = frac + h as f64 - 1.0 - j as f64;
            let v = cutoff * sinc(cutoff * d) * blackman(d / half);
            *w = v as f32;
            sum += v;
        }
        row.iter_mut().for_each(|w| *w = (*w as f64 / sum) as f32);
    }

    let out_len = (input.len() as f64 * ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for n in 0..out_len {
        let t = n as f64 / ratio;
        let base = t.floor() as isize;
        let p = ((t - base as f64) * PHASES as f64).round() as usize;
        let row = &table[p * taps..(p + 1) * taps];
        let first = base - h as isize + 1;
        let mut acc = 0f32;
        if first >= 0 && (first as usize + taps) <= input.len() {
            let window = &input[first as usize..first as usize + taps];
            for (x, w) in window.iter().zip(row) {
                acc += x * w;
            }
        } else {
            for (j, w) in row.iter().enumerate() {
                let k = first + j as isize;
                if k >= 0 && (k as usize) < input.len() {
                    acc += input[k as usize] * w;
                }
            }
        }
        out.push(acc);
    }
    out
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-9 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// Blackman window over [-1, 1].
fn blackman(x: f64) -> f64 {
    if x.abs() >= 1.0 {
        return 0.0;
    }
    let p = std::f64::consts::PI * (x + 1.0);
    0.42 - 0.5 * p.cos() + 0.08 * (2.0 * p).cos()
}

/// Splits long audio at the quietest point near each `max_secs` boundary so
/// no chunk ends mid-word.
pub fn split_for_asr(samples: &[f32], max_secs: f32) -> Vec<&[f32]> {
    let max = (max_secs * TARGET_RATE as f32) as usize;
    let search = (8.0 * TARGET_RATE as f32) as usize;
    let frame = (0.1 * TARGET_RATE as f32) as usize;
    let mut chunks = Vec::new();
    let mut start = 0;
    while samples.len() - start > max {
        let window_end = start + max;
        let window_start = window_end - search;
        let mut best = window_end;
        let mut best_energy = f32::MAX;
        let mut pos = window_start;
        while pos + frame <= window_end {
            let e: f32 = samples[pos..pos + frame].iter().map(|s| s * s).sum();
            if e < best_energy {
                best_energy = e;
                best = pos + frame / 2;
            }
            pos += frame / 2;
        }
        chunks.push(&samples[start..best]);
        start = best;
    }
    chunks.push(&samples[start..]);
    chunks
}

pub fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_keeps_length_and_tone() {
        let from = 48_000;
        let tone: Vec<f32> = (0..from)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / from as f32).sin())
            .collect();
        let out = resample(&tone, from, TARGET_RATE);
        assert_eq!(out.len(), TARGET_RATE as usize);
        // A 440 Hz tone is well under the cutoff, so amplitude survives.
        let p = peak(&out[1000..15000]);
        assert!((p - 1.0).abs() < 0.05, "peak {p}");
    }

    #[test]
    fn resample_removes_content_above_nyquist() {
        let from = 48_000;
        let tone: Vec<f32> = (0..from)
            .map(|i| (2.0 * std::f32::consts::PI * 12_000.0 * i as f32 / from as f32).sin())
            .collect();
        let out = resample(&tone, from, TARGET_RATE);
        assert!(peak(&out[1000..15000]) < 0.05);
    }

    #[test]
    fn split_short_audio_is_one_chunk() {
        let a = vec![0.0; TARGET_RATE as usize * 10];
        assert_eq!(split_for_asr(&a, 60.0).len(), 1);
    }

    #[test]
    fn split_long_audio_at_quiet_point() {
        let mut a = vec![0.5f32; TARGET_RATE as usize * 100];
        let quiet = TARGET_RATE as usize * 55;
        a[quiet..quiet + 3200].iter_mut().for_each(|s| *s = 0.0);
        let chunks = split_for_asr(&a, 60.0);
        assert_eq!(chunks.len(), 2);
        let cut = chunks[0].len();
        assert!(cut > quiet && cut < quiet + 3200, "cut at {cut}");
        assert_eq!(chunks.iter().map(|c| c.len()).sum::<usize>(), a.len());
    }
}
