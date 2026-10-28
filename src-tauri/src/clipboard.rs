//! Puts the transcript into the focused app: snapshot the clipboard, paste,
//! then put the user's clipboard back (every format, not just text, so a
//! copied image or rich text survives).

use std::sync::Mutex;
use std::time::Duration;
use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
    VIRTUAL_KEY,
};

use crate::hotkey::{key_input, wait_for_modifiers_up, INJECTED_TAG};

const CF_UNICODETEXT: u32 = 13;
const VK_CONTROL: u16 = 0x11;
const VK_V: u16 = 0x56;
/// Give up on restoring rather than copy a huge clipboard (big Excel ranges).
const MAX_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;
/// How long the target app gets to read the clipboard before we restore it.
const RESTORE_AFTER: Duration = Duration::from_millis(400);

/// Serialises paste/restore so two dictations can't interleave.
static LOCK: Mutex<()> = Mutex::new(());

struct Snapshot(Vec<(u32, Vec<u8>)>);

/// Formats whose handle is a GDI object or app-private, not global memory.
/// Windows synthesises the GDI ones from CF_DIB anyway.
fn is_global_memory_format(fmt: u32) -> bool {
    !matches!(fmt, 2 | 3 | 9 | 14 | 0x80 | 0x82 | 0x83 | 0x8E | 0x200..=0x3FF)
}

struct Opened;

impl Opened {
    fn new() -> Option<Self> {
        // Another app may hold the clipboard for a moment.
        for _ in 0..20 {
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Some(Opened);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

fn snapshot() -> Option<Snapshot> {
    let _open = Opened::new()?;
    let mut items = Vec::new();
    let mut total = 0usize;
    let mut fmt = 0u32;
    loop {
        fmt = unsafe { EnumClipboardFormats(fmt) };
        if fmt == 0 {
            break;
        }
        if !is_global_memory_format(fmt) {
            continue;
        }
        let Ok(handle) = (unsafe { GetClipboardData(fmt) }) else { continue };
        let hglobal = HGLOBAL(handle.0);
        unsafe {
            let size = GlobalSize(hglobal);
            let ptr = GlobalLock(hglobal) as *const u8;
            if ptr.is_null() {
                continue;
            }
            total += size;
            if total > MAX_SNAPSHOT_BYTES {
                let _ = GlobalUnlock(hglobal);
                log::info!("clipboard over {MAX_SNAPSHOT_BYTES} bytes, won't restore it");
                return None;
            }
            items.push((fmt, std::slice::from_raw_parts(ptr, size).to_vec()));
            let _ = GlobalUnlock(hglobal);
        }
    }
    Some(Snapshot(items))
}

fn put(fmt: u32, bytes: &[u8]) -> bool {
    unsafe {
        let Ok(mem) = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) else { return false };
        let ptr = GlobalLock(mem) as *mut u8;
        if ptr.is_null() {
            let _ = GlobalFree(Some(mem));
            return false;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        let _ = GlobalUnlock(mem);
        if SetClipboardData(fmt, Some(HANDLE(mem.0))).is_err() {
            let _ = GlobalFree(Some(mem));
            return false;
        }
        true
    }
}

/// Keeps our writes out of Win+V history and clipboard managers.
fn mark_private() {
    let fmt = unsafe { RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing")) };
    if fmt != 0 {
        put(fmt, &[0, 0, 0, 0]);
    }
}

fn set_text(text: &str) -> bool {
    let Some(_open) = Opened::new() else { return false };
    if unsafe { EmptyClipboard() }.is_err() {
        return false;
    }
    let utf16: Vec<u8> = text
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(|u| u.to_le_bytes())
        .collect();
    let ok = put(CF_UNICODETEXT, &utf16);
    mark_private();
    ok
}

fn restore(snap: Snapshot) {
    let Some(_open) = Opened::new() else { return };
    if unsafe { EmptyClipboard() }.is_err() {
        return;
    }
    for (fmt, bytes) in &snap.0 {
        put(*fmt, bytes);
    }
    mark_private();
}

fn send(inputs: &[INPUT]) {
    unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
}

fn send_ctrl_v() {
    send(&[
        key_input(VK_CONTROL, false),
        key_input(VK_V, false),
        key_input(VK_V, true),
        key_input(VK_CONTROL, true),
    ]);
}

/// Fallback when the clipboard is locked by another app: type the text as
/// Unicode key events.
fn type_text(text: &str) {
    let inputs: Vec<INPUT> = text
        .encode_utf16()
        .flat_map(|unit| {
            [false, true].map(|up| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: unit,
                        dwFlags: if up { KEYEVENTF_UNICODE | KEYEVENTF_KEYUP } else { KEYEVENTF_UNICODE },
                        time: 0,
                        dwExtraInfo: INJECTED_TAG,
                    },
                },
            })
        })
        .collect();
    send(&inputs);
}

/// Copies plain text to the clipboard without pasting (tray "Copy last").
pub fn copy(text: &str) -> bool {
    let _guard = LOCK.lock().unwrap();
    set_text(text)
}

pub fn paste(text: &str) {
    let guard = LOCK.lock().unwrap();
    wait_for_modifiers_up(Duration::from_millis(1500));
    let snap = snapshot();
    if !set_text(text) {
        log::warn!("clipboard busy, typing the text instead");
        type_text(text);
        return;
    }
    let our_seq = unsafe { GetClipboardSequenceNumber() };
    send_ctrl_v();
    drop(guard);

    let Some(snap) = snap else { return };
    std::thread::spawn(move || {
        std::thread::sleep(RESTORE_AFTER);
        let _guard = LOCK.lock().unwrap();
        // If anything wrote to the clipboard since our paste (the user
        // copied something new), keep that instead of restoring.
        if unsafe { GetClipboardSequenceNumber() } == our_seq {
            restore(snap);
        }
    });
}
