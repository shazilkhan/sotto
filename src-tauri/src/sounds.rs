//! Short start/stop cues, synthesised once and played from memory through
//! winmm (no audio library, no files).

use std::sync::OnceLock;
use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

const RATE: u32 = 44_100;

fn wav(notes: &[(f32, f32)]) -> Vec<u8> {
    let mut pcm: Vec<i16> = Vec::new();
    for &(freq, secs) in notes {
        let n = (secs * RATE as f32) as usize;
        let ramp = (0.006 * RATE as f32) as usize;
        for i in 0..n {
            let env = (i.min(n - i) as f32 / ramp as f32).min(1.0);
            let s = (2.0 * std::f32::consts::PI * freq * i as f32 / RATE as f32).sin();
            pcm.push((s * env * 0.16 * i16::MAX as f32) as i16);
        }
    }
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

static START: OnceLock<Vec<u8>> = OnceLock::new();
static STOP: OnceLock<Vec<u8>> = OnceLock::new();

fn play(data: &'static [u8]) {
    // SND_MEMORY reads straight from the buffer, which lives for the whole
    // process, so async playback is safe.
    unsafe {
        let _ = PlaySoundW(PCWSTR(data.as_ptr() as *const u16), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}

pub fn start() {
    play(START.get_or_init(|| wav(&[(587.0, 0.05), (880.0, 0.07)])));
}

pub fn stop() {
    play(STOP.get_or_init(|| wav(&[(880.0, 0.05), (587.0, 0.07)])));
}
