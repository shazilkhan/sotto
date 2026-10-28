//! Tray icons: the "s" monogram over its underline, drawn by
//! scripts/make-icons.ps1. The underline widens while listening and
//! shortens while transcribing; there's a light and a dark set to match the
//! taskbar.

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

pub fn render(icon: Icon) -> Image<'static> {
    // "light" glyphs are the dark ones drawn for a light taskbar.
    let bytes: &'static [u8] = match (icon, taskbar_is_light()) {
        (Icon::Idle, false) => include_bytes!("../icons/tray/dark-idle.png"),
        (Icon::Recording, false) => include_bytes!("../icons/tray/dark-rec.png"),
        (Icon::Transcribing, false) => include_bytes!("../icons/tray/dark-work.png"),
        (Icon::Unavailable, false) => include_bytes!("../icons/tray/dark-off.png"),
        (Icon::Idle, true) => include_bytes!("../icons/tray/light-idle.png"),
        (Icon::Recording, true) => include_bytes!("../icons/tray/light-rec.png"),
        (Icon::Transcribing, true) => include_bytes!("../icons/tray/light-work.png"),
        (Icon::Unavailable, true) => include_bytes!("../icons/tray/light-off.png"),
    };
    Image::from_bytes(bytes).expect("embedded tray icon is a valid PNG").to_owned()
}
