//! Tray icons, drawn at startup instead of shipped as files. The mark is the
//! logo: a long line (the words) over a short pill (the voice). The pill
//! turns into a wide blue bar while listening and a shorter one while
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

/// Inside a horizontal capsule from x0 to x1 centred on `cy` with radius `r`.
fn in_capsule(x: f32, y: f32, x0: f32, x1: f32, cy: f32, r: f32) -> bool {
    let cx = x.clamp(x0 + r, x1 - r);
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

pub fn render(icon: Icon) -> Image<'static> {
    let light = taskbar_is_light();
    let line: [u8; 3] = match (icon, light) {
        (Icon::Unavailable, _) => [140, 139, 134],
        (_, true) => [12, 12, 13],
        (_, false) => [243, 242, 238],
    };
    let pill: [u8; 3] = match (icon, light) {
        (Icon::Unavailable, _) => [140, 139, 134],
        (_, true) => [50, 54, 245],
        (_, false) => [108, 112, 255],
    };
    let pill_end = match icon {
        Icon::Recording => 27.0,
        Icon::Transcribing => 21.0,
        _ => 16.5,
    };

    let mut rgba = vec![0u8; SIZE * SIZE * 4];
    for py in 0..SIZE {
        for px in 0..SIZE {
            let (mut a_line, mut a_pill) = (0usize, 0usize);
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = px as f32 + (sx as f32 + 0.5) / SS as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / SS as f32;
                    if in_capsule(x, y, 5.0, 27.0, 11.5, 1.75) {
                        a_line += 1;
                    } else if in_capsule(x, y, 5.0, pill_end, 19.5, 3.5) {
                        a_pill += 1;
                    }
                }
            }
            let n = (SS * SS) as f32;
            let (l, p) = (a_line as f32 / n, a_pill as f32 / n);
            let alpha = (l + p).min(1.0);
            let i = (py * SIZE + px) * 4;
            if alpha > 0.0 {
                for c in 0..3 {
                    rgba[i + c] = ((line[c] as f32 * l + pill[c] as f32 * p) / alpha) as u8;
                }
            }
            rgba[i + 3] = (alpha * 255.0) as u8;
        }
    }
    Image::new_owned(rgba, SIZE as u32, SIZE as u32)
}
