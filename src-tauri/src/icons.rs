//! Tray icons, drawn at startup instead of shipped as files: a microphone,
//! plain when idle, on a red disc while recording and amber while
//! transcribing.

use tauri::image::Image;
use windows::core::w;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Idle,
    Recording,
    Transcribing,
    Unavailable,
}

const SIZE: usize = 32;
const SS: usize = 4; // supersampling per axis

fn taskbar_is_light() -> bool {
    let mut value = 0u32;
    let mut len = std::mem::size_of::<u32>() as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut len),
        )
    };
    r.is_ok() && value == 1
}

fn seg_dist(px: f32, py: f32, x: f32, y0: f32, y1: f32) -> f32 {
    let cy = py.clamp(y0, y1);
    ((px - x).powi(2) + (py - cy).powi(2)).sqrt()
}

fn in_mic(x: f32, y: f32) -> bool {
    let capsule = seg_dist(x, y, 16.0, 9.0, 14.0) <= 5.0;
    let r = ((x - 16.0).powi(2) + (y - 13.0).powi(2)).sqrt();
    let arc = y >= 13.0 && (r - 9.0).abs() <= 1.15;
    let stem = (14.9..=17.1).contains(&x) && (22.0..=26.0).contains(&y);
    let base = (11.0..=21.0).contains(&x) && (25.8..=28.0).contains(&y);
    capsule || arc || stem || base
}

fn in_disc(x: f32, y: f32) -> bool {
    ((x - 16.0).powi(2) + (y - 16.0).powi(2)).sqrt() <= 15.5
}

pub fn render(icon: Icon) -> Image<'static> {
    let light = taskbar_is_light();
    let fg: [u8; 4] = match icon {
        Icon::Idle if light => [32, 32, 36, 255],
        Icon::Idle => [240, 240, 244, 255],
        Icon::Unavailable => [140, 140, 148, 200],
        Icon::Recording | Icon::Transcribing => [255, 255, 255, 255],
    };
    let disc: Option<[u8; 4]> = match icon {
        Icon::Recording => Some([229, 57, 53, 255]),
        Icon::Transcribing => Some([245, 158, 11, 255]),
        _ => None,
    };

    let mut rgba = vec![0u8; SIZE * SIZE * 4];
    for py in 0..SIZE {
        for px in 0..SIZE {
            let (mut mic, mut bg) = (0usize, 0usize);
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = px as f32 + (sx as f32 + 0.5) / SS as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / SS as f32;
                    // The glyph shrinks a little inside the disc.
                    let (gx, gy) = if disc.is_some() { (16.0 + (x - 16.0) * 1.35, 16.0 + (y - 16.0) * 1.35) } else { (x, y) };
                    if in_mic(gx, gy) {
                        mic += 1;
                    }
                    if in_disc(x, y) {
                        bg += 1;
                    }
                }
            }
            let n = (SS * SS) as f32;
            let (a_mic, a_bg) = (mic as f32 / n, if disc.is_some() { bg as f32 / n } else { 0.0 });
            let base = disc.unwrap_or([0, 0, 0, 0]);
            let i = (py * SIZE + px) * 4;
            for c in 0..3 {
                let v = fg[c] as f32 * a_mic + base[c] as f32 * (1.0 - a_mic);
                rgba[i + c] = v as u8;
            }
            let alpha = (fg[3] as f32 / 255.0) * a_mic + a_bg * (1.0 - a_mic);
            rgba[i + 3] = (alpha.min(1.0) * 255.0) as u8;
        }
    }
    Image::new_owned(rgba, SIZE as u32, SIZE as u32)
}
