//! Which app the user is dictating into. Captured at hotkey press, because
//! focus can move while they talk. Only the exe name is read, never window
//! titles (they can hold email subjects, file names, chat names).

use windows::core::PWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

/// A readable app name for the history list: "slack.exe" -> "Slack".
pub fn display_name(exe: Option<&str>) -> String {
    let Some(exe) = exe else { return String::new() };
    let stem = exe.strip_suffix(".exe").unwrap_or(exe);
    let known = match stem {
        "chrome" => "Chrome",
        "msedge" => "Edge",
        "firefox" => "Firefox",
        "code" => "VS Code",
        "cursor" => "Cursor",
        "windowsterminal" | "cmd" | "powershell" | "pwsh" => "Terminal",
        "ms-teams" | "teams" => "Teams",
        "winword" => "Word",
        "outlook" | "olk" => "Outlook",
        "whatsapp" => "WhatsApp",
        "explorer" => "Explorer",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    let mut chars = stem.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// Lowercased exe name of the foreground window's process, e.g. "slack.exe".
pub fn foreground_exe() -> Option<String> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(process);
        if !ok {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit('\\').next().map(|name| name.to_lowercase())
    }
}
