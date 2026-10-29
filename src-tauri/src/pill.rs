//! The floating pill: a small always-on-top layered window drawn with
//! Direct2D, not a webview. Idle, it's the logo's underline; while you talk
//! a pill lifts above it and the underline turns blue.
//!
//! Cost rules: it never takes focus (WS_EX_NOACTIVATE, so dragging it can't
//! steal the caret from the text box), repaints only when its state changes,
//! animates at 30 fps only while listening or writing, and hides while a
//! fullscreen app is in front unless you're dictating.

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_numerics::Vector2 as D2D_POINT_2F;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, ID2D1SolidColorBrush, ID2D1StrokeStyle,
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_CAP_STYLE_ROUND, D2D1_DASH_STYLE_SOLID,
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
    D2D1_LINE_JOIN_ROUND, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE, D2D1_ROUNDED_RECT, D2D1_STROKE_STYLE_PROPERTIES,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_MEDIUM,
    DWRITE_TEXT_METRICS, DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, GetDC, GetMonitorInfoW,
    MonitorFromPoint, MonitorFromWindow, ReleaseDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    GetClassNameW, GetCursorPos, GetForegroundWindow, GetMessageW, GetShellWindow, GetWindowRect,
    KillTimer, LoadCursorW, PostMessageW, RegisterClassExW, SetForegroundWindow, SetTimer,
    SetWindowPos, ShowWindow, TrackPopupMenu, TranslateMessage, UpdateLayeredWindow, CS_DBLCLKS,
    EVENT_SYSTEM_FOREGROUND, IDC_ARROW, MA_NOACTIVATE, MF_CHECKED, MF_SEPARATOR, MF_STRING,
    MSG, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_SHOWNOACTIVATE, TPM_BOTTOMALIGN,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, ULW_ALPHA, WINEVENT_OUTOFCONTEXT, WM_APP,
    WM_DISPLAYCHANGE, WM_DPICHANGED, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NULL, WM_RBUTTONUP, WM_TIMER, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

#[derive(Debug, Clone, PartialEq)]
pub enum PillState {
    Idle,
    /// `hands_free`: locked on by a double-tap; a click or tap stops it.
    Listening { muted: u32, hands_free: bool },
    Writing,
    /// Brief confirmation, then back to idle.
    Pasted,
    /// A short line. `error` turns the underline red; `ttl_ms` reverts to
    /// idle after that long (None keeps it up, e.g. download progress).
    Message { text: String, error: bool, ttl_ms: Option<u32> },
}

pub enum PillEvent {
    OpenApp,
    /// Click while hands-free: finish and paste.
    StopListening,
    OpenSettings,
    CopyLast,
    ToggleTalkingOnly,
    ToggleHandsFree,
    Quit,
    Moved { x: i32, y: i32 },
}

struct Shared {
    state: PillState,
    since: Instant,
    mode: String,
    label: String,
    /// Hands-free mode: the hover hint says "tap" instead of "hold".
    tap: bool,
    reset_pos: bool,
    /// A note that arrived mid-dictation, shown once the pill settles.
    pending: Option<String>,
}

const NOTE_MS: u32 = 2600;

/// What the pill shows when it goes back to rest: a waiting note, or idle.
fn settled(s: &mut Shared) -> PillState {
    match s.pending.take() {
        Some(text) => PillState::Message { text, error: false, ttl_ms: Some(NOTE_MS) },
        None => PillState::Idle,
    }
}

static SHARED: Mutex<Option<Shared>> = Mutex::new(None);
static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
static EVENTS: OnceLock<Box<dyn Fn(PillEvent) + Send + Sync>> = OnceLock::new();

const WM_PILL_SYNC: u32 = WM_APP + 1;
/// Lives in the Win32_UI_Controls feature; not worth pulling that in.
const WM_MOUSELEAVE: u32 = 0x02A3;
const TIMER_ANIM: usize = 1;
const TIMER_REVERT: usize = 2;
/// Canvas in DIPs. The underline's centre sits at (W/2, H - BAR_FROM_BOTTOM).
const W: f32 = 360.0;
const H: f32 = 96.0;
const BAR_FROM_BOTTOM: f32 = 18.0;
const BARS: usize = 9;

fn post_sync() {
    let raw = HWND_RAW.load(Ordering::SeqCst);
    if raw != 0 {
        let _ = unsafe { PostMessageW(Some(HWND(raw as *mut _)), WM_PILL_SYNC, WPARAM(0), LPARAM(0)) };
    }
}

fn with_shared<R>(f: impl FnOnce(&mut Shared) -> R) -> Option<R> {
    SHARED.lock().ok()?.as_mut().map(f)
}

pub fn set(state: PillState) {
    let changed = with_shared(|s| {
        let state = if state == PillState::Idle { settled(s) } else { state };
        if s.state == state && !matches!(state, PillState::Pasted) {
            return false;
        }
        s.state = state;
        s.since = Instant::now();
        true
    });
    if changed == Some(true) {
        post_sync();
    }
}

/// A short note such as "Added Supabase to Dictionary". During a
/// dictation it waits until the text is pasted, so it never covers the
/// listening pill.
pub fn note(text: String) {
    let changed = with_shared(|s| {
        if matches!(s.state, PillState::Listening { .. } | PillState::Writing | PillState::Pasted) {
            s.pending = Some(text);
            return false;
        }
        s.state = PillState::Message { text, error: false, ttl_ms: Some(NOTE_MS) };
        s.since = Instant::now();
        true
    });
    if changed == Some(true) {
        post_sync();
    }
}

pub fn set_mode(mode: &str) {
    with_shared(|s| s.mode = mode.to_string());
    post_sync();
}

pub fn set_label(label: &str) {
    with_shared(|s| s.label = label.to_string());
    post_sync();
}

/// `sotto --pill-demo` (sent to the running instance): walks the pill
/// through its states with a fake voice level, for screenshots and QA.
pub fn demo() {
    std::thread::spawn(|| {
        let voice = std::thread::spawn(|| {
            let start = Instant::now();
            while start.elapsed().as_millis() < 2600 {
                let t = start.elapsed().as_secs_f32();
                let level = 0.02 + 0.25 * ((t * 7.0).sin().abs() * (t * 2.3).cos().abs());
                crate::audio::PILL_LEVEL.fetch_max(level.to_bits(), Ordering::Relaxed);
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
        });
        let pause = |ms| std::thread::sleep(std::time::Duration::from_millis(ms));
        set(PillState::Listening { muted: 2, hands_free: true });
        pause(2600);
        let _ = voice.join();
        set(PillState::Writing);
        pause(1400);
        set(PillState::Pasted);
        pause(1600);
        set(PillState::Message { text: "No microphone".into(), error: true, ttl_ms: Some(2600) });
    });
}

pub fn set_tap_mode(on: bool) {
    with_shared(|s| s.tap = on);
    post_sync();
}

pub fn reset_position() {
    with_shared(|s| s.reset_pos = true);
    post_sync();
}

fn color(hex: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a,
    }
}

const INK: u32 = 0x0C0C0D;
const PAPER: u32 = 0xF3F2EE;
const BLUE: u32 = 0x3236F5;
const RED: u32 = 0xD92D20;
const MUTED: u32 = 0xB5B4AF;
const BLUE_LIGHT: u32 = 0x8A8DFF;
const KEY_BG: u32 = 0x232326;

struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    /// The pixels (BGRA, premultiplied, top-down), `w` to a row.
    bits: *const u8,
    w: i32,
    h: i32,
}

impl Surface {
    /// Opacity of one pixel of the last frame, 0 when it's off the surface.
    fn alpha_at(&self, x: i32, y: i32) -> u8 {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return 0;
        }
        unsafe {
            // GDI batches drawing; the pixels are only final after a flush.
            let _ = GdiFlush();
            *self.bits.add(((y * self.w + x) * 4 + 3) as usize)
        }
    }

    fn new(w: i32, h: i32) -> Option<Self> {
        unsafe {
            let dc = CreateCompatibleDC(None);
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let Ok(bitmap) = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
                let _ = DeleteDC(dc);
                return None;
            };
            let old = SelectObject(dc, bitmap.into());
            Some(Self { dc, bitmap, old, bits: bits as *const u8, w, h })
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

struct Fonts {
    body: IDWriteTextFormat,
    small: IDWriteTextFormat,
}

struct Pill {
    hwnd: HWND,
    /// Makes the render target, again whenever a frame fails to draw.
    d2d: ID2D1Factory,
    dw: IDWriteFactory,
    rt: ID2D1DCRenderTarget,
    brush: ID2D1SolidColorBrush,
    round: ID2D1StrokeStyle,
    fonts: Fonts,
    surface: Surface,
    scale: f32,
    /// Underline centre in screen pixels.
    anchor: (i32, i32),
    visible: bool,
    hover: bool,
    tracking: bool,
    press: Option<((i32, i32), (i32, i32))>,
    moved: bool,
    fullscreen: bool,
    animating: bool,
    bars: [f32; BARS],
    /// The step that kept the last frame off the screen, if it failed.
    failing: Option<&'static str>,
}

thread_local! {
    static PILL: RefCell<Option<Pill>> = const { RefCell::new(None) };
}

/// Software rendering: a 360x96 surface is trivial on the CPU, and a
/// hardware target would load the GPU driver (dozens of threads, ~40 MB)
/// into the process and share the GPU with whatever game is running.
fn make_target(d2d: &ID2D1Factory) -> windows::core::Result<(ID2D1DCRenderTarget, ID2D1SolidColorBrush)> {
    let props = D2D1_RENDER_TARGET_PROPERTIES {
        r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
        dpiX: 96.0,
        dpiY: 96.0,
        usage: D2D1_RENDER_TARGET_USAGE_NONE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
    };
    unsafe {
        let rt = d2d.CreateDCRenderTarget(&props)?;
        rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        let brush = rt.CreateSolidColorBrush(&color(INK, 1.0), None)?;
        Ok((rt, brush))
    }
}

fn make_fonts(dw: &IDWriteFactory, scale: f32) -> windows::core::Result<Fonts> {
    unsafe {
        let make = |size: f32| -> windows::core::Result<IDWriteTextFormat> {
            let f = dw.CreateTextFormat(
                w!("Segoe UI Variable Text"),
                None,
                DWRITE_FONT_WEIGHT_MEDIUM,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size * scale,
                w!("en-us"),
            )?;
            f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            Ok(f)
        };
        Ok(Fonts { body: make(13.0)?, small: make(11.5)? })
    }
}

fn work_area_at(x: i32, y: i32) -> RECT {
    unsafe {
        let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut info);
        info.rcWork
    }
}

fn default_anchor(scale: f32) -> (i32, i32) {
    unsafe {
        let mon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut info);
        let r = info.rcWork;
        ((r.left + r.right) / 2, r.bottom - (14.0 * scale) as i32)
    }
}

/// Keeps the pill fully on some screen (screens change; saved spots can vanish).
fn clamp_anchor(x: i32, y: i32, scale: f32) -> (i32, i32) {
    let r = work_area_at(x, y);
    let mx = (40.0 * scale) as i32;
    let top = (64.0 * scale) as i32;
    let bottom = (6.0 * scale) as i32;
    (x.clamp(r.left + mx, r.right - mx), y.clamp(r.top + top, r.bottom - bottom))
}

/// True when `hwnd` covers its whole monitor (a game or a fullscreen video).
fn is_fullscreen(hwnd: HWND) -> bool {
    unsafe {
        if hwnd.is_invalid() || hwnd == GetShellWindow() {
            return false;
        }
        let mut class = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut class) as usize;
        let class = String::from_utf16_lossy(&class[..n]);
        if class == "Progman" || class == "WorkerW" {
            return false;
        }
        let mut r = RECT::default();
        if GetWindowRect(hwnd, &mut r).is_err() {
            return false;
        }
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut info);
        let m = info.rcMonitor;
        r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
    }
}

unsafe extern "system" fn on_foreground(_: HWINEVENTHOOK, _: u32, hwnd: HWND, _: i32, _: i32, _: u32, _: u32) {
    let full = is_fullscreen(hwnd);
    PILL.with(|p| {
        if let Ok(mut p) = p.try_borrow_mut() {
            if let Some(p) = p.as_mut() {
                p.fullscreen = full;
            }
        }
    });
    post_sync();
}

impl Pill {
    fn size_px(&self) -> (i32, i32) {
        ((W * self.scale).round() as i32, (H * self.scale).round() as i32)
    }

    fn top_left(&self) -> (i32, i32) {
        let s = self.scale;
        (self.anchor.0 - (W / 2.0 * s).round() as i32, self.anchor.1 - ((H - BAR_FROM_BOTTOM) * s).round() as i32)
    }

    fn rescale(&mut self, scale: f32) {
        if (scale - self.scale).abs() < 0.01 {
            return;
        }
        self.scale = scale;
        if let Ok(f) = make_fonts(&self.dw, scale) {
            self.fonts = f;
        }
        let (w, h) = self.size_px();
        if let Some(s) = Surface::new(w, h) {
            self.surface = s;
        }
    }

    fn snapshot() -> (PillState, Instant, String, String) {
        with_shared(|s| (s.state.clone(), s.since, s.mode.clone(), s.label.clone()))
            .unwrap_or((PillState::Idle, Instant::now(), "always".into(), "Right Ctrl".into()))
    }

    fn hint() -> &'static str {
        if with_shared(|s| s.tap).unwrap_or(false) { "tap to talk" } else { "hold to talk" }
    }

    /// Applies the shared state: visibility, timers, then a repaint.
    fn sync(&mut self) {
        if with_shared(|s| std::mem::take(&mut s.reset_pos)) == Some(true) {
            self.anchor = default_anchor(self.scale);
        }
        let (state, _, mode, _) = Self::snapshot();
        let active = state != PillState::Idle;
        let show = match mode.as_str() {
            "never" => false,
            "talking" => active,
            _ => true,
        } && (active || !self.fullscreen);

        let animate = show && matches!(state, PillState::Listening { .. } | PillState::Writing);
        unsafe {
            if animate && !self.animating {
                SetTimer(Some(self.hwnd), TIMER_ANIM, 33, None);
                self.bars = [0.0; BARS];
            } else if !animate && self.animating {
                let _ = KillTimer(Some(self.hwnd), TIMER_ANIM);
            }
            self.animating = animate;
            let _ = KillTimer(Some(self.hwnd), TIMER_REVERT);
            let ttl = match &state {
                PillState::Pasted => Some(1200),
                PillState::Message { ttl_ms, .. } => *ttl_ms,
                _ => None,
            };
            if let Some(ms) = ttl {
                SetTimer(Some(self.hwnd), TIMER_REVERT, ms, None);
            }
        }
        if show {
            self.draw();
        }
        if show != self.visible {
            self.visible = show;
            unsafe {
                let _ = ShowWindow(self.hwnd, if show { SW_SHOWNOACTIVATE } else { SW_HIDE });
            }
        }
    }

    fn text_width(&self, text: &str, small: bool) -> f32 {
        let fmt = if small { &self.fonts.small } else { &self.fonts.body };
        let wide: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let Ok(layout) = self.dw.CreateTextLayout(&wide, fmt, 10_000.0, 1_000.0) else { return 0.0 };
            let mut m = DWRITE_TEXT_METRICS::default();
            if layout.GetMetrics(&mut m).is_err() {
                return 0.0;
            }
            m.width
        }
    }

    /// Draws `text` with its left edge at `x` (px), vertically centred on `cy`.
    fn text(&self, text: &str, small: bool, x: f32, cy: f32, rgb: u32, a: f32) -> f32 {
        let fmt = if small { &self.fonts.small } else { &self.fonts.body };
        let wide: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let Ok(layout) = self.dw.CreateTextLayout(&wide, fmt, 10_000.0, 1_000.0) else { return 0.0 };
            let mut m = DWRITE_TEXT_METRICS::default();
            let _ = layout.GetMetrics(&mut m);
            self.brush.SetColor(&color(rgb, a));
            self.rt.DrawTextLayout(D2D_POINT_2F { X: x, Y: cy - m.height / 2.0 }, &layout, &self.brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            m.width
        }
    }

    fn rrect(&self, l: f32, t: f32, r: f32, b: f32, radius: f32, rgb: u32, a: f32) {
        unsafe {
            self.brush.SetColor(&color(rgb, a));
            let rr = D2D1_ROUNDED_RECT { rect: D2D_RECT_F { left: l, top: t, right: r, bottom: b }, radiusX: radius, radiusY: radius };
            self.rt.FillRoundedRectangle(&rr, &self.brush);
        }
    }

    fn draw(&mut self) {
        let (state, since, _, label) = Self::snapshot();
        if self.animating {
            let peak = f32::from_bits(crate::audio::PILL_LEVEL.swap(0, Ordering::Relaxed));
            let db = 20.0 * peak.max(1e-5).log10();
            let level = ((db + 50.0) / 42.0).clamp(0.0, 1.0);
            self.bars.rotate_left(1);
            self.bars[BARS - 1] = level;
        }
        let s = self.scale;
        let (w, h) = self.size_px();
        let cx = w as f32 / 2.0;
        let bar_y = (H - BAR_FROM_BOTTOM) * s;

        unsafe {
            let rect = RECT { left: 0, top: 0, right: w, bottom: h };
            if let Err(e) = self.rt.BindDC(self.surface.dc, &rect) {
                return self.failed("BindDC", e.to_string());
            }
            self.rt.BeginDraw();
            self.rt.Clear(Some(&color(0, 0.0)));

            // Invisible halo so the thin idle bar is easy to grab.
            self.brush.SetColor(&color(0, 1.0 / 255.0));
            self.rt.FillRectangle(&D2D_RECT_F { left: cx - 44.0 * s, top: bar_y - 16.0 * s, right: cx + 44.0 * s, bottom: bar_y + 14.0 * s }, &self.brush);

            let show_cap = state != PillState::Idle || self.hover;
            if show_cap {
                self.draw_capsule(&state, since, &label, cx, bar_y);
            }

            // The underline.
            let (bw, bh, rgb) = match &state {
                PillState::Idle => (44.0, 6.0, INK),
                PillState::Listening { .. } => (28.0, 5.0, BLUE),
                PillState::Writing => {
                    let t = since.elapsed().as_secs_f32();
                    let breathe = 0.5 + 0.5 * (t * std::f32::consts::TAU / 1.1).sin();
                    (12.0 + 22.0 * breathe, 5.0, BLUE)
                }
                PillState::Pasted => (28.0, 5.0, INK),
                PillState::Message { error, .. } => (28.0, 5.0, if *error { RED } else { INK }),
            };
            let (bw, bh, ring) = (bw * s, bh * s, 1.5 * s);
            let ring_a = if matches!(state, PillState::Idle | PillState::Pasted) { 0.6 } else { 0.35 };
            self.rrect(cx - bw / 2.0 - ring, bar_y - bh / 2.0 - ring, cx + bw / 2.0 + ring, bar_y + bh / 2.0 + ring, bh / 2.0 + ring, PAPER, ring_a);
            self.rrect(cx - bw / 2.0, bar_y - bh / 2.0, cx + bw / 2.0, bar_y + bh / 2.0, bh / 2.0, rgb, 1.0);

            if let Err(e) = self.rt.EndDraw(None, None) {
                return self.failed("EndDraw", e.to_string());
            }
            // The underline is always drawn solid, so a clear pixel at its
            // centre means the frame came out blank.
            if self.surface.alpha_at(cx as i32, bar_y as i32) == 0 {
                return self.failed("drawing", "the frame came out blank".into());
            }
            let (x, y) = self.top_left();
            let dst = POINT { x, y };
            let src = POINT { x: 0, y: 0 };
            let size = SIZE { cx: w, cy: h };
            let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
            let screen = GetDC(None);
            let shown = UpdateLayeredWindow(self.hwnd, Some(screen), Some(&dst), Some(&size), Some(self.surface.dc), Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA);
            ReleaseDC(None, screen);
            if let Err(e) = shown {
                return self.failed("UpdateLayeredWindow", e.to_string());
            }
        }
        if self.failing.take().is_some() {
            log::info!("floating pill is drawing again");
        }
    }

    /// A frame that never reached the screen. Logged once per cause, and the
    /// drawing surface is rebuilt so the next frame starts clean.
    fn failed(&mut self, step: &'static str, why: String) {
        if self.failing != Some(step) {
            log::warn!("floating pill: {step} failed ({why}); rebuilding its drawing surface");
            self.failing = Some(step);
        }
        self.rebuild();
    }

    /// Fresh drawing surface and render target, after a failed frame or a
    /// display change (a game switching resolution, a monitor waking up).
    fn rebuild(&mut self) {
        let (w, h) = self.size_px();
        if let Some(s) = Surface::new(w, h) {
            self.surface = s;
        }
        match make_target(&self.d2d) {
            Ok((rt, brush)) => (self.rt, self.brush) = (rt, brush),
            Err(e) => log::warn!("floating pill: no render target ({e})"),
        }
    }

    fn draw_capsule(&self, state: &PillState, since: Instant, label: &str, cx: f32, bar_y: f32) {
        let s = self.scale;
        let pad = 16.0 * s;
        let gap = 10.0 * s;
        let bars_w = (BARS as f32 * 3.0 + (BARS - 1) as f32 * 3.0) * s;

        // Measure first so the capsule fits its content.
        let elapsed = since.elapsed().as_secs();
        let time = format!("{}:{:02}", elapsed / 60, elapsed % 60);
        let content_w = match state {
            PillState::Idle => {
                let key = self.text_width(label, true) + 14.0 * s;
                key + gap + self.text_width(Pill::hint(), false)
            }
            PillState::Listening { muted, hands_free } => {
                let mut cw = bars_w + gap + self.text_width(&time, true);
                if *hands_free {
                    cw += 10.0 * s + gap;
                }
                if *muted > 0 {
                    cw += gap * 2.0 + 1.0 * s + self.text_width(&format!("{muted} muted"), true);
                }
                cw
            }
            PillState::Writing => self.text_width("Writing", false),
            PillState::Pasted => 15.0 * s + 8.0 * s + self.text_width("Pasted", false),
            PillState::Message { text, .. } => self.text_width(text, false),
        };
        let cap_h = 38.0 * s;
        let cw = content_w + pad * 2.0;
        let l = (cx - cw / 2.0).round();
        let r = l + cw;
        let b = bar_y - 3.0 * s - 6.0 * s;
        let t = b - cap_h;
        let cy = (t + b) / 2.0;

        // Soft shadow, then the capsule and its hairline edge.
        for (i, a) in [0.07f32, 0.045, 0.025].iter().enumerate() {
            let grow = (i as f32 + 1.0) * 2.5 * s;
            self.rrect(l - grow, t - grow + 4.0 * s, r + grow, b + grow + 4.0 * s, cap_h / 2.0 + grow, INK, *a);
        }
        self.rrect(l - 1.0 * s, t - 1.0 * s, r + 1.0 * s, b + 1.0 * s, cap_h / 2.0 + 1.0 * s, PAPER, 0.12);
        self.rrect(l, t, r, b, cap_h / 2.0, INK, 1.0);

        let mut x = l + pad;
        match state {
            PillState::Idle => {
                let kw = self.text_width(label, true) + 14.0 * s;
                self.rrect(x, cy - 11.0 * s, x + kw, cy + 11.0 * s, 5.0 * s, KEY_BG, 1.0);
                self.text(label, true, x + 7.0 * s, cy, PAPER, 1.0);
                x += kw + gap;
                self.text(Pill::hint(), false, x, cy, PAPER, 1.0);
            }
            PillState::Listening { muted, hands_free } => {
                if *hands_free {
                    // Stop square: click the pill (or tap the shortcut) to finish.
                    self.rrect(x, cy - 5.0 * s, x + 10.0 * s, cy + 5.0 * s, 2.0 * s, BLUE_LIGHT, 1.0);
                    x += 10.0 * s + gap;
                }
                for (i, level) in self.bars.iter().enumerate() {
                    let bh = (4.0 + 12.0 * level) * s;
                    let bx = x + i as f32 * 6.0 * s;
                    self.rrect(bx, cy - bh / 2.0, bx + 3.0 * s, cy + bh / 2.0, 1.5 * s, PAPER, 1.0);
                }
                x += bars_w + gap;
                x += self.text(&time, true, x, cy, MUTED, 1.0);
                if *muted > 0 {
                    x += gap;
                    self.rrect(x, cy - 8.0 * s, x + 1.0 * s, cy + 8.0 * s, 0.0, 0x2E2E32, 1.0);
                    x += 1.0 * s + gap;
                    self.text(&format!("{muted} muted"), true, x, cy, MUTED, 1.0);
                }
            }
            PillState::Writing => {
                self.text("Writing", false, x, cy, PAPER, 1.0);
            }
            PillState::Pasted => {
                unsafe {
                    self.brush.SetColor(&color(PAPER, 1.0));
                    let p = |dx: f32, dy: f32| D2D_POINT_2F { X: x + dx * s, Y: cy + dy * s };
                    self.rt.DrawLine(p(1.5, 0.5), p(5.5, 4.5), &self.brush, 2.4 * s, &self.round);
                    self.rt.DrawLine(p(5.5, 4.5), p(13.5, -4.5), &self.brush, 2.4 * s, &self.round);
                }
                x += 15.0 * s + 8.0 * s;
                self.text("Pasted", false, x, cy, PAPER, 1.0);
            }
            PillState::Message { text, .. } => {
                self.text(text, false, x, cy, PAPER, 1.0);
            }
        }
    }

    fn move_to(&mut self, anchor: (i32, i32)) {
        self.anchor = anchor;
        let (x, y) = self.top_left();
        unsafe {
            let _ = SetWindowPos(self.hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }
}

fn emit(ev: PillEvent) {
    if let Some(f) = EVENTS.get() {
        f(ev);
    }
}

fn cursor() -> (i32, i32) {
    let mut p = POINT::default();
    let _ = unsafe { GetCursorPos(&mut p) };
    (p.x, p.y)
}

fn show_menu(hwnd: HWND) {
    let (talking_only, tap) = with_shared(|s| (s.mode == "talking", s.tap)).unwrap_or((false, false));
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING, 1, w!("Open Sotto"));
        let _ = AppendMenuW(menu, MF_STRING, 2, w!("Copy last transcript"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let checked = |on: bool| if on { MF_STRING | MF_CHECKED } else { MF_STRING };
        let _ = AppendMenuW(menu, checked(tap), 5, w!("Hands-free mode"));
        let _ = AppendMenuW(menu, checked(talking_only), 3, w!("Show only while talking"));
        let _ = AppendMenuW(menu, MF_STRING, 4, w!("Quit Sotto"));
        let (x, y) = cursor();
        // The menu needs the foreground to close on an outside click.
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_NONOTIFY, x, y, Some(0), hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        match cmd.0 {
            1 => emit(PillEvent::OpenApp),
            2 => emit(PillEvent::CopyLast),
            3 => emit(PillEvent::ToggleTalkingOnly),
            5 => emit(PillEvent::ToggleHandsFree),
            4 => emit(PillEvent::Quit),
            _ => {}
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    if msg == WM_RBUTTONUP {
        show_menu(hwnd); // outside any borrow: the menu runs a nested message loop
        return LRESULT(0);
    }
    if msg == WM_LBUTTONDBLCLK {
        emit(PillEvent::OpenApp);
        return LRESULT(0);
    }
    let mut after: Option<PillEvent> = None;
    let handled = PILL.with(|cell| {
        // Re-entrant calls (SetWindowPos, ShowWindow...) fall through to the default.
        let Ok(mut guard) = cell.try_borrow_mut() else { return false };
        let Some(p) = guard.as_mut() else { return false };
        match msg {
            WM_PILL_SYNC => p.sync(),
            WM_TIMER if wparam.0 == TIMER_ANIM => p.draw(),
            WM_TIMER if wparam.0 == TIMER_REVERT => {
                let _ = KillTimer(Some(hwnd), TIMER_REVERT);
                with_shared(|s| {
                    if matches!(s.state, PillState::Pasted | PillState::Message { .. }) {
                        s.state = settled(s);
                        s.since = Instant::now();
                    }
                });
                p.sync();
            }
            WM_LBUTTONDOWN => {
                p.press = Some((cursor(), p.anchor));
                p.moved = false;
                SetCapture(hwnd);
            }
            WM_MOUSEMOVE => {
                if let Some((start, anchor)) = p.press {
                    let (x, y) = cursor();
                    let (dx, dy) = (x - start.0, y - start.1);
                    if p.moved || dx.abs() + dy.abs() > 3 {
                        p.moved = true;
                        p.move_to((anchor.0 + dx, anchor.1 + dy));
                    }
                } else if !p.hover {
                    p.hover = true;
                    if !p.tracking {
                        let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                        p.tracking = TrackMouseEvent(&mut tme).is_ok();
                    }
                    p.draw();
                }
            }
            WM_MOUSELEAVE => {
                p.tracking = false;
                if p.hover {
                    p.hover = false;
                    p.draw();
                }
            }
            WM_LBUTTONUP => {
                if p.press.take().is_some() {
                    if p.moved {
                        let a = clamp_anchor(p.anchor.0, p.anchor.1, p.scale);
                        p.move_to(a);
                        after = Some(PillEvent::Moved { x: a.0, y: a.1 });
                    } else {
                        let (state, ..) = Pill::snapshot();
                        if matches!(state, PillState::Message { error: true, .. }) {
                            after = Some(PillEvent::OpenSettings);
                        } else if matches!(state, PillState::Listening { hands_free: true, .. }) {
                            after = Some(PillEvent::StopListening);
                        }
                    }
                    let _ = ReleaseCapture();
                }
            }
            WM_DPICHANGED => {
                let dpi = (wparam.0 & 0xffff) as f32;
                p.rescale(dpi / 96.0);
                p.sync();
            }
            WM_DISPLAYCHANGE => {
                let a = clamp_anchor(p.anchor.0, p.anchor.1, p.scale);
                p.move_to(a);
                p.rebuild();
                p.sync();
            }
            _ => return false,
        }
        true
    });
    if let Some(ev) = after {
        emit(ev);
    }
    if handled {
        LRESULT(0)
    } else {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

/// Creates the pill on its own thread. `pos` is a saved underline position.
pub fn start(mode: &str, label: &str, tap: bool, pos: Option<(i32, i32)>, events: Box<dyn Fn(PillEvent) + Send + Sync>) {
    *SHARED.lock().unwrap() = Some(Shared {
        state: PillState::Idle,
        since: Instant::now(),
        mode: mode.to_string(),
        label: label.to_string(),
        tap,
        reset_pos: false,
        pending: None,
    });
    let _ = EVENTS.set(events);
    std::thread::Builder::new()
        .name("pill".into())
        .spawn(move || unsafe {
            let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            if let Err(e) = run(pos) {
                log::error!("floating pill unavailable: {e}");
            }
        })
        .expect("spawn pill thread");
}

unsafe fn run(pos: Option<(i32, i32)>) -> windows::core::Result<()> {
    let module = GetModuleHandleW(None)?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_DBLCLKS,
        lpfnWndProc: Some(wndproc),
        hInstance: module.into(),
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        lpszClassName: w!("SottoPill"),
        ..Default::default()
    };
    RegisterClassExW(&class);
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        w!("SottoPill"),
        w!("Sotto"),
        WS_POPUP,
        0,
        0,
        1,
        1,
        None,
        None,
        Some(module.into()),
        None,
    )?;
    let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;

    let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
    let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
    let (rt, brush) = make_target(&d2d)?;
    let round = d2d.CreateStrokeStyle(
        &D2D1_STROKE_STYLE_PROPERTIES {
            startCap: D2D1_CAP_STYLE_ROUND,
            endCap: D2D1_CAP_STYLE_ROUND,
            dashCap: D2D1_CAP_STYLE_ROUND,
            lineJoin: D2D1_LINE_JOIN_ROUND,
            miterLimit: 10.0,
            dashStyle: D2D1_DASH_STYLE_SOLID,
            dashOffset: 0.0,
        },
        None,
    )?;
    let fonts = make_fonts(&dw, scale)?;
    let surface = Surface::new((W * scale).round() as i32, (H * scale).round() as i32)
        .ok_or_else(windows::core::Error::from_thread)?;

    let anchor = match pos {
        Some((x, y)) => clamp_anchor(x, y, scale),
        None => default_anchor(scale),
    };
    let mut pill = Pill {
        hwnd,
        d2d,
        dw,
        rt,
        brush,
        round,
        fonts,
        surface,
        scale,
        anchor,
        visible: false,
        hover: false,
        tracking: false,
        press: None,
        moved: false,
        fullscreen: is_fullscreen(GetForegroundWindow()),
        animating: false,
        bars: [0.0; BARS],
        failing: None,
    };
    // The window was created on the primary screen; re-read DPI where it lands.
    pill.move_to(anchor);
    let dpi = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
    pill.rescale(dpi);
    PILL.with(|cell| *cell.borrow_mut() = Some(pill));
    HWND_RAW.store(hwnd.0 as isize, Ordering::SeqCst);

    let _hook = SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(on_foreground), 0, 0, WINEVENT_OUTOFCONTEXT);
    post_sync();
    log::info!("floating pill ready");

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    Ok(())
}
