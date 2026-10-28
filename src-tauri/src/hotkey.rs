//! Push-to-talk hotkey via a low-level keyboard hook.
//!
//! `RegisterHotKey` can't do modifier-only chords like Ctrl+Win and never
//! reports key-up, so we watch raw key events instead. Windows calls the hook
//! for every keystroke system-wide and drops it if it is slow, so the
//! callback only updates a tiny key set and posts to a channel.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, KBDLLHOOKSTRUCT, MSG,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::engine::Msg;

/// Tags input we inject ourselves so the hook can ignore it.
pub const INJECTED_TAG: usize = 0x5077_0770;

/// Set while a hands-free dictation is running, so Esc can cancel it.
/// Esc is ignored the rest of the time.
pub static HANDS_FREE: AtomicBool = AtomicBool::new(false);
const VK_ESCAPE: u32 = 0x1B;
const VK_RETURN: u32 = 0x0D;

const VK_LSHIFT: u32 = 0xA0;
const VK_RSHIFT: u32 = 0xA1;
const VK_LCONTROL: u32 = 0xA2;
const VK_RCONTROL: u32 = 0xA3;
const VK_LMENU: u32 = 0xA4;
const VK_RMENU: u32 = 0xA5;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;
/// Unassigned key. Tapping it while Win is held stops the Start menu from
/// opening when Win is released (the same trick AutoHotkey uses).
const VK_MENU_MASK: u16 = 0xE8;

const CTRL: &[u32] = &[VK_LCONTROL, VK_RCONTROL];
const WIN: &[u32] = &[VK_LWIN, VK_RWIN];
const ALT: &[u32] = &[VK_LMENU, VK_RMENU];
const SHIFT: &[u32] = &[VK_LSHIFT, VK_RSHIFT];

/// (id, label, keys). Each entry in `keys` is a set of interchangeable keys.
pub const PRESETS: &[(&str, &str, &[&[u32]])] = &[
    ("ctrl+win", "Ctrl + Win", &[CTRL, WIN]),
    ("right-ctrl", "Right Ctrl", &[&[VK_RCONTROL]]),
    ("right-alt", "Right Alt", &[&[VK_RMENU]]),
    ("ctrl+alt", "Ctrl + Alt", &[CTRL, ALT]),
    ("ctrl+shift", "Ctrl + Shift", &[CTRL, SHIFT]),
];

pub fn label_for(id: &str) -> &'static str {
    PRESETS.iter().find(|p| p.0 == id).unwrap_or(&PRESETS[0]).1
}

pub fn combo_for(id: &str) -> &'static [&'static [u32]] {
    PRESETS.iter().find(|p| p.0 == id).unwrap_or(&PRESETS[0]).2
}

pub fn combo_uses_win(id: &str) -> bool {
    combo_for(id).iter().any(|keys| keys.iter().any(|k| WIN.contains(k)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Pressed,
    Released,
    /// Another key joined the chord, so it was a normal shortcut.
    Cancelled,
    /// Esc during a hands-free dictation.
    Escape,
}

#[derive(PartialEq)]
enum Phase {
    Idle,
    Active,
    /// Fired or cancelled; wait until every chord key is up before re-arming.
    WaitRelease,
}

struct HookState {
    combo: &'static [&'static [u32]],
    held: Vec<u32>,
    phase: Phase,
    tx: Sender<Msg>,
}

impl HookState {
    fn in_combo(&self, vk: u32) -> bool {
        self.combo.iter().any(|keys| keys.contains(&vk))
    }

    fn on_key(&mut self, vk: u32, down: bool) -> Option<HotkeyEvent> {
        if down {
            if self.held.contains(&vk) {
                return None; // auto-repeat
            }
            self.held.push(vk);
        } else {
            self.held.retain(|&k| k != vk);
        }
        // A key-up can go missing (Win+L, UAC prompts), so trust the async
        // state for every key except the one this event is about.
        self.held.retain(|&k| k == vk || is_down(k));

        let in_combo = self.in_combo(vk);
        let satisfied = self.combo.iter().all(|keys| keys.iter().any(|k| self.held.contains(k)));
        let only_combo = self.held.iter().all(|&k| self.in_combo(k));

        let event = match self.phase {
            Phase::Idle if down && satisfied && only_combo => {
                self.phase = Phase::Active;
                Some(HotkeyEvent::Pressed)
            }
            Phase::Active if down && !in_combo => {
                self.phase = Phase::WaitRelease;
                Some(HotkeyEvent::Cancelled)
            }
            Phase::Active if !down && !satisfied => {
                self.phase = Phase::WaitRelease;
                Some(HotkeyEvent::Released)
            }
            _ => None,
        };
        if self.phase == Phase::WaitRelease && !self.held.iter().any(|&k| self.in_combo(k)) {
            self.phase = Phase::Idle;
        }
        event
    }
}

static STATE: Mutex<Option<HookState>> = Mutex::new(None);

fn is_down(vk: u32) -> bool {
    unsafe { GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let msg = wparam.0 as u32;
        let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
        if down && kb.vkCode == VK_RETURN && kb.dwExtraInfo != INJECTED_TAG {
            // Returns at once unless a paste is being watched for fixes.
            crate::learn::before_enter();
        }
        if down && kb.vkCode == VK_ESCAPE && HANDS_FREE.load(Ordering::Relaxed) {
            if let Ok(guard) = STATE.try_lock() {
                if let Some(state) = guard.as_ref() {
                    let _ = state.tx.send(Msg::Hotkey(HotkeyEvent::Escape));
                }
            }
        }
        if (down || up) && kb.dwExtraInfo != INJECTED_TAG {
            if let Ok(mut guard) = STATE.try_lock() {
                if let Some(state) = guard.as_mut() {
                    if let Some(ev) = state.on_key(kb.vkCode, down) {
                        let _ = state.tx.send(Msg::Hotkey(ev));
                    }
                }
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// Installs the hook on its own thread with a message loop. Idle cost is a
/// thread blocked in `GetMessageW`.
pub fn start(tx: Sender<Msg>, preset: &str) {
    *STATE.lock().unwrap() = Some(HookState {
        combo: combo_for(preset),
        held: Vec::new(),
        phase: Phase::Idle,
        tx,
    });
    std::thread::Builder::new()
        .name("hotkey".into())
        .spawn(|| unsafe {
            // Input lag is the one thing a dictation app must never cause.
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
            let module = GetModuleHandleW(None).ok().map(|m| HINSTANCE(m.0));
            let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), module, 0) {
                Ok(h) => h,
                Err(e) => {
                    log::error!("keyboard hook failed: {e}");
                    return;
                }
            };
            log::info!("keyboard hook installed");
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                DispatchMessageW(&msg);
            }
            let _ = windows::Win32::UI::WindowsAndMessaging::UnhookWindowsHookEx(hook);
        })
        .expect("spawn hotkey thread");
}

pub fn set_preset(preset: &str) {
    if let Some(state) = STATE.lock().unwrap().as_mut() {
        state.combo = combo_for(preset);
        state.held.clear();
        state.phase = Phase::Idle;
    }
}

pub fn key_input(vk: u16, up: bool) -> INPUT {
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: INJECTED_TAG,
            },
        },
    }
}

pub fn suppress_start_menu() {
    let inputs = [key_input(VK_MENU_MASK, false), key_input(VK_MENU_MASK, true)];
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
}

/// Waits (bounded) until Win, Alt and Shift are up, so an injected Ctrl+V
/// isn't read as Win+V or Ctrl+Shift+V.
pub fn wait_for_modifiers_up(max: std::time::Duration) {
    let start = std::time::Instant::now();
    let keys = [VK_LWIN, VK_RWIN, VK_LMENU, VK_RMENU, VK_LSHIFT, VK_RSHIFT];
    while keys.iter().any(|&k| is_down(k)) && start.elapsed() < max {
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(preset: &str) -> HookState {
        let (tx, _rx) = std::sync::mpsc::channel();
        HookState { combo: combo_for(preset), held: vec![], phase: Phase::Idle, tx }
    }

    // `on_key` prunes with GetAsyncKeyState, which reads "up" for every key
    // in a test process, so these tests only press one chord key at a time.
    #[test]
    fn single_key_press_and_release() {
        let mut s = state("right-ctrl");
        assert_eq!(s.on_key(VK_RCONTROL, true), Some(HotkeyEvent::Pressed));
        assert_eq!(s.on_key(VK_RCONTROL, true), None, "auto-repeat is ignored");
        assert_eq!(s.on_key(VK_RCONTROL, false), Some(HotkeyEvent::Released));
        assert_eq!(s.on_key(VK_RCONTROL, true), Some(HotkeyEvent::Pressed), "re-arms");
    }

    #[test]
    fn other_key_cancels() {
        let mut s = state("right-ctrl");
        assert_eq!(s.on_key(VK_RCONTROL, true), Some(HotkeyEvent::Pressed));
        assert_eq!(s.on_key(0x43, true), Some(HotkeyEvent::Cancelled));
        assert_eq!(s.on_key(0x43, false), None);
        assert_eq!(s.on_key(VK_RCONTROL, false), None);
        assert_eq!(s.on_key(VK_RCONTROL, true), Some(HotkeyEvent::Pressed));
    }
}
