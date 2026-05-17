
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TrackMouseEvent, TRACKMOUSEEVENT, TRACKMOUSEEVENT_FLAGS,
};
use windows::Win32::UI::Shell::ExtractIconExW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::diagnose;
use crate::localization::{self, LanguageId, Strings};
use crate::models::AppUsageData;

// ── GDI+ flat API (gdiplus.dll) ──────────────────────────────────────────────
// Declared manually to avoid adding Win32_Graphics_GdiPlus to Cargo features.

#[repr(C)]
struct GdipStartupInput {
    version: u32,
    _debug_cb: *const std::ffi::c_void,
    suppress_bg_thread: i32,
    suppress_ext_codecs: i32,
}

#[link(name = "gdiplus")]
extern "system" {
    fn GdiplusStartup(token: *mut usize, input: *const GdipStartupInput, output: *mut std::ffi::c_void) -> u32;
    fn GdiplusShutdown(token: usize);
    fn GdipCreateFromHDC(hdc: HDC, g: *mut *mut std::ffi::c_void) -> i32;
    fn GdipDeleteGraphics(g: *mut std::ffi::c_void) -> i32;
    fn GdipSetSmoothingMode(g: *mut std::ffi::c_void, mode: i32) -> i32;
    fn GdipCreateSolidFill(color: u32, brush: *mut *mut std::ffi::c_void) -> i32;
    fn GdipDeleteBrush(brush: *mut std::ffi::c_void) -> i32;
    fn GdipCreatePath(mode: i32, path: *mut *mut std::ffi::c_void) -> i32;
    fn GdipDeletePath(path: *mut std::ffi::c_void) -> i32;
    fn GdipAddPathArcI(path: *mut std::ffi::c_void, x: i32, y: i32, w: i32, h: i32, start: f32, sweep: f32) -> i32;
    fn GdipClosePathFigure(path: *mut std::ffi::c_void) -> i32;
    fn GdipFillPath(g: *mut std::ffi::c_void, brush: *mut std::ffi::c_void, path: *mut std::ffi::c_void) -> i32;
    fn GdipFillEllipseI(g: *mut std::ffi::c_void, brush: *mut std::ffi::c_void, x: i32, y: i32, w: i32, h: i32) -> i32;
    fn GdipCreatePen1(color: u32, width: f32, unit: i32, pen: *mut *mut std::ffi::c_void) -> i32;
    fn GdipDeletePen(pen: *mut std::ffi::c_void) -> i32;
    fn GdipDrawPath(g: *mut std::ffi::c_void, pen: *mut std::ffi::c_void, path: *mut std::ffi::c_void) -> i32;
}

const GDIP_SMOOTHING_ANTIALIAS: i32 = 4;
const GDIP_FILL_WINDING: i32 = 1;
const GDIP_UNIT_PIXEL: i32 = 2;

static GDIPLUS_TOKEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn init_gdiplus() {
    let input = GdipStartupInput { version: 1, _debug_cb: std::ptr::null(), suppress_bg_thread: 0, suppress_ext_codecs: 0 };
    let mut token: usize = 0;
    unsafe { GdiplusStartup(&mut token, &input, std::ptr::null_mut()); }
    GDIPLUS_TOKEN.store(token, std::sync::atomic::Ordering::Relaxed);
}

fn shutdown_gdiplus() {
    let token = GDIPLUS_TOKEN.load(std::sync::atomic::Ordering::Relaxed);
    if token != 0 { unsafe { GdiplusShutdown(token); } }
}

// GDI+ ARGB: 0xAARRGGBB
fn to_gdip_argb(c: Color) -> u32 {
    0xFF00_0000 | ((c.r as u32) << 16) | ((c.g as u32) << 8) | c.b as u32
}

unsafe fn gdip_build_round_path(path: *mut std::ffi::c_void, l: i32, t: i32, r: i32, b: i32, radius: i32) {
    let d = radius * 2;
    GdipAddPathArcI(path, l,     t,     d, d, 180.0, 90.0);
    GdipAddPathArcI(path, r - d, t,     d, d, 270.0, 90.0);
    GdipAddPathArcI(path, r - d, b - d, d, d,   0.0, 90.0);
    GdipAddPathArcI(path, l,     b - d, d, d,  90.0, 90.0);
    GdipClosePathFigure(path);
}

/// Anti-aliased filled rounded rectangle.
unsafe fn gdip_fill_rounded(hdc: HDC, l: i32, t: i32, r: i32, b: i32, radius: i32, color: Color) {
    let mut g: *mut std::ffi::c_void = std::ptr::null_mut();
    if GdipCreateFromHDC(hdc, &mut g) != 0 { return; }
    GdipSetSmoothingMode(g, GDIP_SMOOTHING_ANTIALIAS);
    let mut brush: *mut std::ffi::c_void = std::ptr::null_mut();
    if GdipCreateSolidFill(to_gdip_argb(color), &mut brush) == 0 {
        let mut path: *mut std::ffi::c_void = std::ptr::null_mut();
        if GdipCreatePath(GDIP_FILL_WINDING, &mut path) == 0 {
            gdip_build_round_path(path, l, t, r, b, radius);
            GdipFillPath(g, brush, path);
            GdipDeletePath(path);
        }
        GdipDeleteBrush(brush);
    }
    GdipDeleteGraphics(g);
}

/// Anti-aliased rounded rectangle border.
unsafe fn gdip_stroke_rounded(hdc: HDC, l: i32, t: i32, r: i32, b: i32, radius: i32, color: Color, stroke: f32) {
    let mut g: *mut std::ffi::c_void = std::ptr::null_mut();
    if GdipCreateFromHDC(hdc, &mut g) != 0 { return; }
    GdipSetSmoothingMode(g, GDIP_SMOOTHING_ANTIALIAS);
    let mut pen: *mut std::ffi::c_void = std::ptr::null_mut();
    if GdipCreatePen1(to_gdip_argb(color), stroke, GDIP_UNIT_PIXEL, &mut pen) == 0 {
        let mut path: *mut std::ffi::c_void = std::ptr::null_mut();
        if GdipCreatePath(GDIP_FILL_WINDING, &mut path) == 0 {
            gdip_build_round_path(path, l, t, r, b, radius);
            GdipDrawPath(g, pen, path);
            GdipDeletePath(path);
        }
        GdipDeletePen(pen);
    }
    GdipDeleteGraphics(g);
}

/// Anti-aliased filled ellipse.
unsafe fn gdip_fill_ellipse(hdc: HDC, l: i32, t: i32, r: i32, b: i32, color: Color) {
    let mut g: *mut std::ffi::c_void = std::ptr::null_mut();
    if GdipCreateFromHDC(hdc, &mut g) != 0 { return; }
    GdipSetSmoothingMode(g, GDIP_SMOOTHING_ANTIALIAS);
    let mut brush: *mut std::ffi::c_void = std::ptr::null_mut();
    if GdipCreateSolidFill(to_gdip_argb(color), &mut brush) == 0 {
        GdipFillEllipseI(g, brush, l, t, r - l, b - t);
        GdipDeleteBrush(brush);
    }
    GdipDeleteGraphics(g);
}

// ── DWM corner preference (Windows 11+) ─────────────────────────────────────
#[link(name = "dwmapi")]
extern "system" {
    fn DwmSetWindowAttribute(hwnd: HWND, attr: u32, data: *const std::ffi::c_void, size: u32) -> i32;
}

const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
const DWMWCP_ROUND: u32 = 2;
const DWMWA_SYSTEMBACKDROP_TYPE: u32 = 38;
const DWMSBT_AUTO: u32 = 0;

unsafe fn apply_panel_background(hwnd: HWND, bg: PanelBackground) {
    let style = GetWindowLongW(hwnd, GWL_EXSTYLE);
    match bg {
        PanelBackground::Solid => {
            SetWindowLongW(hwnd, GWL_EXSTYLE, style & !(WS_EX_LAYERED.0 as i32));
        }
        PanelBackground::Translucent => {
            SetWindowLongW(hwnd, GWL_EXSTYLE, style | WS_EX_LAYERED.0 as i32);
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 242, LWA_ALPHA);
        }
    }
    // Ensure no stale DWM backdrop
    let sbt = DWMSBT_AUTO;
    DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &sbt as *const u32 as *const _, 4);
}

use crate::native_interop::{
    self, Color, TIMER_COUNTDOWN, TIMER_PANEL_SHIMMER, TIMER_POLL, TIMER_RESET_POLL,
    TIMER_UPDATE_CHECK, WM_APP_TRAY,
    WM_APP_USAGE_UPDATED,
};
use crate::poller;
use crate::theme;
use crate::tray_icon;
use crate::updater::{self, InstallChannel, ReleaseDescriptor, UpdateCheckResult};

/// Wrapper to make HWND sendable across threads (safe for PostMessage usage)
#[derive(Clone, Copy)]
struct SendHwnd(isize);

unsafe impl Send for SendHwnd {}

impl SendHwnd {
    fn from_hwnd(hwnd: HWND) -> Self {
        Self(hwnd.0 as isize)
    }
    fn to_hwnd(self) -> HWND {
        HWND(self.0 as *mut _)
    }
}

/// Shared application state
struct AppState {
    hwnd: SendHwnd,
    taskbar_hwnd: Option<HWND>,
    tray_notify_hwnd: Option<HWND>,
    win_event_hook: Option<HWINEVENTHOOK>,
    is_dark: bool,
    embedded: bool,
    language_override: Option<LanguageId>,
    language: LanguageId,
    install_channel: InstallChannel,

    session_percent: f64,
    session_text: String,
    session_resets_at: Option<SystemTime>,
    weekly_percent: f64,
    weekly_text: String,
    show_claude_code: bool,

    data: Option<AppUsageData>,

    poll_interval_ms: u32,
    compound_countdown: bool,
    retry_count: u32,
    force_notify_auth_error: bool,
    auth_error_paused_polling: bool,
    auth_watch_mode: poller::CredentialWatchMode,
    auth_watch_snapshot: poller::CredentialWatchSnapshot,
    last_poll_ok: bool,
    last_poll_at: Option<SystemTime>,
    last_poll_status: Option<UsagePollStatus>,
    update_status: UpdateStatus,
    last_update_check_unix: Option<u64>,

    tray_offset: i32,
    dragging: bool,
    drag_start_mouse_x: i32,
    drag_start_offset: i32,

    widget_visible: bool,
    panel_background: PanelBackground,
    panel_pinned: bool,
    panel_pinned_x: Option<i32>,
    panel_pinned_y: Option<i32>,
    panel_menu_open: bool,
    popup_hwnd: Option<HWND>,
    popup_text: String,
    mouse_over_widget: bool,
    panel: PanelState,
    shimmer_phase: bool,
}

#[derive(Clone, Debug)]
enum UpdateStatus {
    Idle,
    Checking,
    Applying,
    UpToDate,
    Available(ReleaseDescriptor),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PanelButtonId {
    Refresh,
    Settings,
    Pin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum PanelBackground {
    #[default]
    Solid,
    Translucent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PanelButtonVisualState {
    Normal,
    Hot,
    Pressed,
    Disabled,
}

#[derive(Clone, Debug)]
struct PanelButton {
    id: PanelButtonId,
    rect: RECT,
    label: String,
    accessible_label: String,
    enabled: bool,
    selected: bool,
}

#[derive(Clone, Debug, Default)]
struct PanelState {
    hwnd: Option<HWND>,
    visible: bool,
    hot_button: Option<PanelButtonId>,
    pressed_button: Option<PanelButtonId>,
    buttons: Vec<PanelButton>,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
struct PanelModelSection {
    name: &'static str,
    session: PanelUsageWindow,
    weekly: PanelUsageWindow,
    session_state: SessionState,
    issue: Option<PanelIssue>,
    user_label: Option<String>,
    message_count: Option<u32>,
    token_count: Option<u64>,
    email: Option<String>,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
struct PanelUsageWindow {
    percentage: Option<f64>,
    reset_time: String,
    resets_at: Option<SystemTime>,
    status: PanelUsageStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PanelIssue {
    MissingCredentials,
    TokenExpired,
    Network,
    Partial,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PanelUsageStatus {
    Normal,
    Caution,
    NearLimit,
    AtLimit,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionState {
    PlentyLeft,
    GoingSteady,
    SlowingDown,
    Capped,
}

fn state_from_utilization(pct: Option<f64>) -> SessionState {
    match pct {
        Some(p) if p >= 100.0 => SessionState::Capped,
        Some(p) if p >= 85.0  => SessionState::SlowingDown,
        Some(p) if p >= 40.0  => SessionState::GoingSteady,
        Some(_)                => SessionState::PlentyLeft,
        None                   => SessionState::PlentyLeft,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UsagePollStatus {
    Success,
    AuthRequired,
    NoCredentials,
    TokenExpired,
    RequestFailed,
}

const RETRY_BASE_MS: u32 = 30_000; // 30 seconds

const POLL_1_MIN: u32 = 60_000;
const POLL_5_MIN: u32 = 300_000;
const POLL_15_MIN: u32 = 900_000;
const POLL_1_HOUR: u32 = 3_600_000;

// Menu item IDs for update frequency
const IDM_FREQ_1MIN: u16 = 10;
const IDM_FREQ_5MIN: u16 = 11;
const IDM_FREQ_15MIN: u16 = 12;
const IDM_FREQ_1HOUR: u16 = 13;
const IDM_START_WITH_WINDOWS: u16 = 20;
const IDM_RESET_POSITION: u16 = 30;
const IDM_VERSION_ACTION: u16 = 31;
const IDM_LANG_SYSTEM: u16 = 40;
const IDM_LANG_ENGLISH: u16 = 41;
const IDM_LANG_DUTCH: u16 = 42;
const IDM_LANG_SPANISH: u16 = 43;
const IDM_LANG_FRENCH: u16 = 44;
const IDM_LANG_GERMAN: u16 = 45;
const IDM_LANG_JAPANESE: u16 = 46;
const IDM_LANG_KOREAN: u16 = 47;
const IDM_LANG_TRADITIONAL_CHINESE: u16 = 48;
const IDM_MODEL_CLAUDE_CODE: u16 = 60;
const IDM_FORMAT_LONG: u16 = 70;
const IDM_FORMAT_SHORT: u16 = 71;
const IDM_PANEL_BG_SOLID: u16 = 80;
const IDM_PANEL_BG_TRANSLUCENT: u16 = 81;

const DIVIDER_HIT_ZONE: i32 = 13; // LEFT_DIVIDER_W + DIVIDER_RIGHT_MARGIN

const WM_DPICHANGED_MSG: u32 = 0x02E0;
const WM_APP_UPDATE_CHECK_COMPLETE: u32 = WM_APP + 2;

const TME_LEAVE: TRACKMOUSEEVENT_FLAGS = TRACKMOUSEEVENT_FLAGS(0x00000002);
const WM_MOUSELEAVE: u32 = 0x02A3;
const TRAY_ICON_UPDATE_REPOSITION_SUPPRESS_MS: u64 = 750;
const PANEL_REOPEN_SUPPRESS_MS: u64 = 200;

static SUPPRESS_TRAY_REPOSITION_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);
static SUPPRESS_PANEL_REOPEN_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

/// Current system DPI (96 = 100% scaling, 144 = 150%, 192 = 200%, etc.)
static CURRENT_DPI: AtomicU32 = AtomicU32::new(96);

/// Scale a base pixel value (designed at 96 DPI) to the current DPI.
fn sc(px: i32) -> i32 {
    let dpi = CURRENT_DPI.load(Ordering::Relaxed);
    (px as f64 * dpi as f64 / 96.0).round() as i32
}

/// Re-query the monitor DPI for our window and update the cached value.
/// Uses GetDpiForWindow which returns the live DPI (unlike GetDpiForSystem
/// which is cached at process startup and never changes).
fn refresh_dpi() {
    let hwnd = {
        let state = lock_state();
        state.as_ref().map(|s| s.hwnd.to_hwnd())
    };
    if let Some(hwnd) = hwnd {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if dpi > 0 {
            CURRENT_DPI.store(dpi, Ordering::Relaxed);
        }
    }
}

fn load_embedded_app_icons() -> (HICON, HICON) {
    unsafe {
        let mut exe_buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
        if len == 0 {
            return (HICON::default(), HICON::default());
        }

        let mut large_icon = HICON::default();
        let mut small_icon = HICON::default();
        let extracted = ExtractIconExW(
            PCWSTR::from_raw(exe_buf.as_ptr()),
            0,
            Some(&mut large_icon),
            Some(&mut small_icon),
            1,
        );

        if extracted == 0 {
            (HICON::default(), HICON::default())
        } else {
            (large_icon, small_icon)
        }
    }
}

unsafe impl Send for AppState {}

static STATE: Mutex<Option<AppState>> = Mutex::new(None);

/// Lock STATE safely, recovering from poisoned mutex
fn lock_state() -> MutexGuard<'static, Option<AppState>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn settings_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata)
        .join("ClaudeCodeUsageMonitor")
        .join("settings.json")
}

#[derive(Debug, Serialize, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    tray_offset: i32,
    #[serde(default = "default_poll_interval")]
    poll_interval_ms: u32,
    #[serde(default = "default_compound_countdown")]
    compound_countdown: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_update_check_unix: Option<u64>,
    #[serde(default = "default_widget_visible")]
    widget_visible: bool,
    #[serde(default = "default_show_claude_code")]
    show_claude_code: bool,
    #[serde(default)]
    panel_background: PanelBackground,
    #[serde(default)]
    panel_pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    panel_pinned_x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    panel_pinned_y: Option<i32>,
}

impl Default for SettingsFile {
    fn default() -> Self {
        Self {
            tray_offset: 0,
            poll_interval_ms: default_poll_interval(),
            compound_countdown: default_compound_countdown(),
            language: None,
            last_update_check_unix: None,
            widget_visible: true,
            show_claude_code: true,
            panel_background: PanelBackground::Solid,
            panel_pinned: false,
            panel_pinned_x: None,
            panel_pinned_y: None,
        }
    }
}

fn default_poll_interval() -> u32 {
    POLL_15_MIN
}

fn default_compound_countdown() -> bool {
    true
}

fn default_widget_visible() -> bool {
    true
}

fn default_show_claude_code() -> bool {
    true
}

fn load_settings() -> SettingsFile {
    let content = match std::fs::read_to_string(settings_path()) {
        Ok(c) => c,
        Err(_) => return SettingsFile::default(),
    };
    // Fast path: all fields parse correctly.
    if let Ok(mut s) = serde_json::from_str::<SettingsFile>(&content) {
        sanitize_settings(&mut s);
        return s;
    }
    // Slow path: recover field-by-field so one bad value doesn't reset all settings.
    let raw: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return SettingsFile::default(),
    };
    let d = SettingsFile::default();
    let mut s = SettingsFile {
        tray_offset: raw["tray_offset"]
            .as_i64()
            .map(|v| v as i32)
            .unwrap_or(d.tray_offset),
        poll_interval_ms: raw["poll_interval_ms"]
            .as_u64()
            .map(|v| v as u32)
            .unwrap_or(d.poll_interval_ms),
        compound_countdown: raw["compound_countdown"]
            .as_bool()
            .unwrap_or(d.compound_countdown),
        language: raw["language"].as_str().map(|s| s.to_string()),
        last_update_check_unix: raw["last_update_check_unix"].as_u64(),
        widget_visible: raw["widget_visible"].as_bool().unwrap_or(d.widget_visible),
        show_claude_code: raw["show_claude_code"]
            .as_bool()
            .unwrap_or(d.show_claude_code),
        panel_background: raw["panel_background"]
            .as_str()
            .and_then(|v| serde_json::from_value(serde_json::Value::String(v.to_string())).ok())
            .unwrap_or(d.panel_background),
        panel_pinned: raw["panel_pinned"].as_bool().unwrap_or(false),
        panel_pinned_x: raw["panel_pinned_x"].as_i64().map(|v| v as i32),
        panel_pinned_y: raw["panel_pinned_y"].as_i64().map(|v| v as i32),
    };
    sanitize_settings(&mut s);
    s
}

fn sanitize_settings(s: &mut SettingsFile) {
    // Clamp poll interval to the four allowed values.
    const VALID: [u32; 4] = [POLL_1_MIN, POLL_5_MIN, POLL_15_MIN, POLL_1_HOUR];
    if !VALID.contains(&s.poll_interval_ms) {
        s.poll_interval_ms = default_poll_interval();
    }
}

fn save_settings(settings: &SettingsFile) {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(path, json);
    }
}

fn save_state_settings() {
    let state = lock_state();
    if let Some(s) = state.as_ref() {
        save_settings(&SettingsFile {
            tray_offset: s.tray_offset,
            poll_interval_ms: s.poll_interval_ms,
            compound_countdown: s.compound_countdown,
            language: s
                .language_override
                .map(|language| language.code().to_string()),
            last_update_check_unix: s.last_update_check_unix,
            widget_visible: s.widget_visible,
            show_claude_code: s.show_claude_code,
            panel_background: s.panel_background,
            panel_pinned: s.panel_pinned,
            panel_pinned_x: s.panel_pinned_x,
            panel_pinned_y: s.panel_pinned_y,
        });
    }
}

fn tray_icon_data_from_state() -> Vec<tray_icon::TrayIconData> {
    let state = lock_state();
    match state.as_ref() {
        Some(s) if s.last_poll_ok => {
            let mut icons = Vec::new();
            if s.show_claude_code {
                let reset_line = s
                    .data
                    .as_ref()
                    .and_then(|d| d.claude_code.as_ref())
                    .map(|u| reset_tooltip_suffix(u.session.resets_at, u.weekly.resets_at))
                    .unwrap_or_default();
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Claude,
                    percent: Some(s.session_percent),
                    tooltip: format!(
                        "{} 5h: {} | 7d: {}{}",
                        s.language.strings().claude_code_model,
                        s.session_text,
                        s.weekly_text,
                        reset_line
                    ),
                });
            }
            icons
        }
        Some(s) => {
            let mut icons = Vec::new();
            if s.show_claude_code {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Claude,
                    percent: None,
                    tooltip: s.language.strings().window_title.to_string(),
                });
            }
            icons
        }
        None => Vec::new(),
    }
}

fn sync_tray_icons(hwnd: HWND) {
    let icons = tray_icon_data_from_state();
    tray_icon::sync(hwnd, &icons);
}

fn toggle_widget_visibility(hwnd: HWND) {
    let new_visible = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.widget_visible = !s.widget_visible;
            s.widget_visible
        } else {
            return;
        }
    };
    save_state_settings();
    unsafe {
        if new_visible {
            position_at_taskbar();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            render_layered();
        } else {
            hide_panel();
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn update_check_interval() -> Duration {
    Duration::from_secs(24 * 60 * 60)
}

fn auto_update_check_due(last_update_check_unix: Option<u64>) -> bool {
    let Some(last_update_check_unix) = last_update_check_unix else {
        return true;
    };

    now_unix_secs().saturating_sub(last_update_check_unix) >= update_check_interval().as_secs()
}

fn schedule_auto_update_check(hwnd: HWND) {
    let delay_ms = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return;
        };

        if auto_update_check_due(s.last_update_check_unix) {
            None
        } else {
            let elapsed = now_unix_secs().saturating_sub(s.last_update_check_unix.unwrap_or(0));
            let remaining_secs = update_check_interval().as_secs().saturating_sub(elapsed);
            Some((remaining_secs.saturating_mul(1000)).min(u32::MAX as u64) as u32)
        }
    };

    unsafe {
        let _ = KillTimer(hwnd, TIMER_UPDATE_CHECK);
        if let Some(delay_ms) = delay_ms {
            SetTimer(hwnd, TIMER_UPDATE_CHECK, delay_ms.max(1), None);
        }
    }
}

fn refresh_usage_texts(state: &mut AppState) {
    if !state.last_poll_ok {
        return;
    }

    let strings = state.language.strings();
    let Some(data) = state.data.as_ref() else {
        return;
    };

    let compound = state.compound_countdown;

    if let Some(claude_code) = data.claude_code.as_ref() {
        state.session_text = poller::format_line(&claude_code.session, strings, compound);
        state.session_resets_at = claude_code.session.resets_at;
        state.weekly_text = poller::format_line(&claude_code.weekly, strings, compound);
    } else if state.show_claude_code {
        state.session_text = "!".to_string();
        state.session_resets_at = None;
        state.weekly_text = "!".to_string();
    }
}

fn set_window_title(hwnd: HWND, strings: Strings) {
    unsafe {
        let title = native_interop::wide_str(strings.window_title);
        let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(title.as_ptr()));
    }
}

fn show_info_message(hwnd: HWND, title: &str, message: &str) {
    unsafe {
        let title_wide = native_interop::wide_str(title);
        let message_wide = native_interop::wide_str(message);
        let _ = MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

fn show_error_message(hwnd: HWND, title: &str, message: &str) {
    unsafe {
        let title_wide = native_interop::wide_str(title);
        let message_wide = native_interop::wide_str(message);
        let _ = MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn show_update_prompt(hwnd: HWND, strings: Strings, release: &ReleaseDescriptor) -> bool {
    let message = strings
        .update_prompt_now
        .replace("{version}", &release.latest_version);

    unsafe {
        let title_wide = native_interop::wide_str(strings.update_available);
        let message_wide = native_interop::wide_str(&message);
        MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_YESNO | MB_ICONQUESTION,
        ) == IDYES
    }
}

fn apply_language_to_state(state: &mut AppState, language_override: Option<LanguageId>) {
    state.language_override = language_override;
    state.language = localization::resolve_language(language_override);
    set_window_title(state.hwnd.to_hwnd(), state.language.strings());
    if let Some(panel_hwnd) = state.panel.hwnd {
        set_panel_title(panel_hwnd, state.language.strings());
    }
    refresh_usage_texts(state);
}

fn update_language_change() -> bool {
    let mut state = lock_state();
    let Some(app_state) = state.as_mut() else {
        return false;
    };

    if app_state.language_override.is_some() {
        return false;
    }

    let new_language = localization::detect_system_language();
    if new_language == app_state.language {
        return false;
    }

    apply_language_to_state(app_state, None);
    true
}

fn version_action_label(
    strings: Strings,
    language: LanguageId,
    install_channel: InstallChannel,
    status: &UpdateStatus,
) -> String {
    let current = env!("CARGO_PKG_VERSION");
    match status {
        UpdateStatus::Idle => format!("v{current} - {}", strings.check_for_updates),
        UpdateStatus::Checking => format!("v{current} - {}", strings.checking_for_updates),
        UpdateStatus::Applying => format!("v{current} - {}", strings.applying_update),
        UpdateStatus::UpToDate => format!("v{current} - {}", strings.up_to_date_short),
        UpdateStatus::Available(release) => match install_channel {
            InstallChannel::Portable => {
                format!(
                    "v{current} - {} v{}",
                    strings.update_to, release.latest_version
                )
            }
            InstallChannel::Winget => format!(
                "v{current} - {} v{}",
                localization::update_via_winget(language),
                release.latest_version
            ),
        },
    }
}

fn begin_update_check(hwnd: HWND, interactive: bool) {
    let send_hwnd = SendHwnd::from_hwnd(hwnd);
    let (strings, install_channel) = {
        let mut state = lock_state();
        let Some(app_state) = state.as_mut() else {
            return;
        };

        if matches!(
            app_state.update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        ) {
            if interactive {
                show_info_message(
                    hwnd,
                    app_state.language.strings().updates,
                    app_state.language.strings().update_in_progress,
                );
            }
            return;
        }

        app_state.update_status = UpdateStatus::Checking;
        (app_state.language.strings(), app_state.install_channel)
    };

    std::thread::spawn(move || {
        let hwnd = send_hwnd.to_hwnd();
        let checked_at = now_unix_secs();
        match updater::check_for_updates() {
            Ok(UpdateCheckResult::UpToDate) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::UpToDate;
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive {
                    show_info_message(hwnd, strings.updates, strings.up_to_date);
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
            Ok(UpdateCheckResult::Available(release)) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Available(release.clone());
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive && show_update_prompt(hwnd, strings, &release) {
                    match install_channel {
                        InstallChannel::Portable => begin_update_apply(hwnd, release),
                        InstallChannel::Winget => begin_winget_update(hwnd),
                    }
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
            Err(error) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Idle;
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive {
                    let message = format!("{}.\n\n{}", strings.update_failed, error);
                    show_error_message(hwnd, strings.updates, &message);
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
        }
    });
}

fn begin_update_apply(hwnd: HWND, release: ReleaseDescriptor) {
    let send_hwnd = SendHwnd::from_hwnd(hwnd);
    let strings = {
        let mut state = lock_state();
        let Some(app_state) = state.as_mut() else {
            return;
        };

        if matches!(
            app_state.update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        ) {
            show_info_message(
                hwnd,
                app_state.language.strings().updates,
                app_state.language.strings().update_in_progress,
            );
            return;
        }

        app_state.update_status = UpdateStatus::Applying;
        app_state.language.strings()
    };

    std::thread::spawn(move || {
        let hwnd = send_hwnd.to_hwnd();
        match updater::begin_self_update(&release) {
            Ok(()) => unsafe {
                let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
            },
            Err(error) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Available(release);
                    }
                }
                let message = format!("{}.\n\n{}", strings.update_failed, error);
                show_error_message(hwnd, strings.updates, &message);
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
        }
    });
}

fn begin_winget_update(hwnd: HWND) {
    let strings = {
        let state = lock_state();
        state.as_ref().map(|s| s.language.strings())
    }
    .unwrap_or(LanguageId::English.strings());

    match updater::begin_winget_update() {
        Ok(()) => unsafe {
            let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        },
        Err(error) => {
            let message = format!("{}.\n\n{}", strings.update_failed, error);
            show_error_message(hwnd, strings.updates, &message);
        }
    }
}

const STARTUP_REGISTRY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_REGISTRY_KEY: &str = "ClaudeCodeUsageMonitor";

/// Returns true only if the startup registry value points to this executable.
fn is_startup_enabled() -> bool {
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);
        let key_name = native_interop::wide_str(STARTUP_REGISTRY_KEY);

        let mut hkey = HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_READ,
            &mut hkey,
        );
        if result.is_err() {
            return false;
        }

        // Query the size of the value
        let mut data_size: u32 = 0;
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(key_name.as_ptr()),
            None,
            None,
            None,
            Some(&mut data_size),
        );
        if result.is_err() || data_size == 0 {
            let _ = RegCloseKey(hkey);
            return false;
        }

        // Read the value
        let mut buf = vec![0u8; data_size as usize];
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(key_name.as_ptr()),
            None,
            None,
            Some(buf.as_mut_ptr()),
            Some(&mut data_size),
        );
        let _ = RegCloseKey(hkey);
        if result.is_err() {
            return false;
        }

        // Convert the registry value (UTF-16) to a string
        let wide_slice =
            std::slice::from_raw_parts(buf.as_ptr() as *const u16, data_size as usize / 2);
        let reg_value = String::from_utf16_lossy(wide_slice)
            .trim_end_matches('\0')
            .to_string();

        // Get the current executable path
        let mut exe_buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
        if len == 0 {
            return false;
        }
        let current_exe = String::from_utf16_lossy(&exe_buf[..len]);

        // Case-insensitive comparison (Windows paths are case-insensitive)
        reg_value.eq_ignore_ascii_case(&current_exe)
    }
}

fn set_startup_enabled(enable: bool) {
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);

        let mut hkey = HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        );
        if result.is_err() {
            return;
        }

        let key_name = native_interop::wide_str(STARTUP_REGISTRY_KEY);

        if enable {
            let mut exe_buf = [0u16; 260];
            let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
            if len > 0 {
                // Write the wide string including null terminator
                let byte_len = ((len + 1) * 2) as u32;
                let _ = RegSetValueExW(
                    hkey,
                    PCWSTR::from_raw(key_name.as_ptr()),
                    0,
                    REG_SZ,
                    Some(std::slice::from_raw_parts(
                        exe_buf.as_ptr() as *const u8,
                        byte_len as usize,
                    )),
                );
            }
        } else {
            let _ = RegDeleteValueW(hkey, PCWSTR::from_raw(key_name.as_ptr()));
        }

        let _ = RegCloseKey(hkey);
    }
}

// Dimensions matching the C# version
const SEGMENT_W: i32 = 16;
const SEGMENT_GAP: i32 = 1;
const SEGMENT_COUNT: i32 = 10;

const LEFT_DIVIDER_W: i32 = 3;
const DIVIDER_RIGHT_MARGIN: i32 = 10;
const BAR_RIGHT_MARGIN: i32 = 4;
const RIGHT_MARGIN: i32 = 1;
const WIDGET_HEIGHT: i32 = 46;

fn active_model_count() -> i32 {
    1
}

fn row_bar_segment_count(active_models: i32) -> i32 {
    if active_models > 1 {
        5
    } else {
        SEGMENT_COUNT
    }
}

fn total_widget_width_for(active_models: i32) -> i32 {
    let bar_segments = row_bar_segment_count(active_models);
    let bar_w = (sc(SEGMENT_W) + sc(SEGMENT_GAP)) * bar_segments - sc(SEGMENT_GAP);

    sc(LEFT_DIVIDER_W)
        + sc(DIVIDER_RIGHT_MARGIN)
        + bar_w
        + sc(BAR_RIGHT_MARGIN)
        + sc(RIGHT_MARGIN)
}

fn total_widget_width_for_state(_state: &AppState) -> i32 {
    total_widget_width_for(active_model_count())
}

fn total_widget_width() -> i32 {
    total_widget_width_for(active_model_count())
}

fn claude_accent_color() -> Color {
    Color::from_hex("#D97757")
}


const POPUP_CLASS: &str = "ClaudeCodeUsagePopup";
const POPUP_PAD_X: i32 = 6;
const POPUP_PAD_Y: i32 = 4;
const PANEL_CLASS: &str = "ClaudeCodeUsagePanel";
const PANEL_SINGLE_MODEL_W: i32 = 390;
const PANEL_TWO_MODEL_W: i32 = 390;
// PANEL_PAD + PANEL_HEADER_H + 1 (separator gap) + PANEL_CARD_H [* n_models + 14 gap between models]
const PANEL_SINGLE_MODEL_H: i32 = PANEL_PAD + PANEL_HEADER_H + 1 + PANEL_CARD_H;           // 283
const PANEL_TWO_MODEL_H:    i32 = PANEL_PAD + PANEL_HEADER_H + 1 + PANEL_CARD_H * 2 + 14;  // 529
const PANEL_PAD: i32 = 5;
const PANEL_HEADER_H: i32 = 40;
const PANEL_BUTTON_H: i32 = 28;
const PANEL_BUTTON_W: i32 = 32;
const PANEL_CORNER_RADIUS: i32 = 10;
const PANEL_HERO_H:   i32 = 120;
const PANEL_WEEKLY_H: i32 = 62;
const PANEL_CARD_H: i32 = PANEL_HERO_H + PANEL_WEEKLY_H; // 182

#[derive(Clone, Copy)]
#[allow(dead_code)]
struct PanelColors {
    bg: Color,
    bg_soft: Color,
    border: Color,
    divider: Color,
    text: Color,
    muted: Color,
    dim: Color,
    badge_border: Color,
    badge_text: Color,
    red: Color,
    red_soft: Color,
    amber: Color,
    amber_soft: Color,
    green: Color,
    green_soft: Color,
}

impl PanelColors {
    fn new(is_dark: bool) -> Self {
        if is_dark {
            Self {
                bg:           Color::from_hex("#1C1C1E"),
                bg_soft:      Color::from_hex("#2C2C2E"),
                border:       Color::from_hex("#3A3A3C"),
                divider:      Color::from_hex("#2C2C2E"),
                text:         Color::from_hex("#F2F2F2"),
                muted:        Color::from_hex("#8E8E93"),
                dim:          Color::from_hex("#636366"),
                badge_border: Color::from_hex("#9E5020"),
                badge_text:   Color::from_hex("#C87838"),
                red:          Color::from_hex("#C93636"),
                red_soft:     Color::from_hex("#3A1A1A"),
                amber:        Color::from_hex("#D96B37"),
                amber_soft:   Color::from_hex("#3A2210"),
                green:        Color::from_hex("#8FCB9B"),
                green_soft:   Color::from_hex("#1A2E20"),
            }
        } else {
            Self {
                bg:           Color::from_hex("#F2F2F7"),
                bg_soft:      Color::from_hex("#E5E5EA"),
                border:       Color::from_hex("#C6C6C8"),
                divider:      Color::from_hex("#D1D1D6"),
                text:         Color::from_hex("#1C1C1E"),
                muted:        Color::from_hex("#6C6C70"),
                dim:          Color::from_hex("#8E8E93"),
                badge_border: Color::from_hex("#C4651A"),
                badge_text:   Color::from_hex("#8B4010"),
                red:          Color::from_hex("#C93636"),
                red_soft:     Color::from_hex("#FCEAEA"),
                amber:        Color::from_hex("#D96B37"),
                amber_soft:   Color::from_hex("#FDF0E7"),
                green:        Color::from_hex("#2F7D48"),
                green_soft:   Color::from_hex("#EBF7EE"),
            }
        }
    }

    #[allow(dead_code)]
    fn session_accent(&self, state: SessionState) -> Color {
        match state {
            SessionState::Capped      => self.red,
            SessionState::SlowingDown => self.amber,
            SessionState::GoingSteady => self.green,
            SessionState::PlentyLeft  => self.green,
        }
    }

    #[allow(dead_code)]
    fn session_soft(&self, state: SessionState) -> Color {
        match state {
            SessionState::Capped      => self.red_soft,
            SessionState::SlowingDown => self.amber_soft,
            SessionState::GoingSteady => self.green_soft,
            SessionState::PlentyLeft  => self.green_soft,
        }
    }
}

unsafe fn tooltip_font() -> HFONT {
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    let _ = SystemParametersInfoW(
        SPI_GETNONCLIENTMETRICS,
        ncm.cbSize,
        Some((&mut ncm as *mut NONCLIENTMETRICSW).cast()),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    );
    CreateFontIndirectW(&ncm.lfStatusFont)
}

#[allow(dead_code)]
unsafe fn panel_small_font() -> HFONT {
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    let _ = SystemParametersInfoW(
        SPI_GETNONCLIENTMETRICS,
        ncm.cbSize,
        Some((&mut ncm as *mut NONCLIENTMETRICSW).cast()),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    );
    let mut lf = ncm.lfStatusFont;
    lf.lfHeight = -sc(11);
    lf.lfWeight = 400;
    CreateFontIndirectW(&lf)
}

unsafe fn panel_large_font() -> HFONT {
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    let _ = SystemParametersInfoW(
        SPI_GETNONCLIENTMETRICS,
        ncm.cbSize,
        Some((&mut ncm as *mut NONCLIENTMETRICSW).cast()),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    );
    let mut lf = ncm.lfStatusFont;
    lf.lfHeight = -sc(20);
    lf.lfWeight = 300;
    CreateFontIndirectW(&lf)
}

unsafe fn panel_bold_font() -> HFONT {
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    let _ = SystemParametersInfoW(
        SPI_GETNONCLIENTMETRICS,
        ncm.cbSize,
        Some((&mut ncm as *mut NONCLIENTMETRICSW).cast()),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    );
    let mut lf = ncm.lfStatusFont;
    lf.lfWeight = 700;
    CreateFontIndirectW(&lf)
}

unsafe fn panel_mono_sys_font() -> HFONT {
    // Consolas at system status-font size, weight 600 — used for weekly % and other inline mono
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    let _ = SystemParametersInfoW(
        SPI_GETNONCLIENTMETRICS,
        ncm.cbSize,
        Some((&mut ncm as *mut NONCLIENTMETRICSW).cast()),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    );
    use windows::Win32::Graphics::Gdi::CreateFontW;
    let face: Vec<u16> = "Consolas\0".encode_utf16().collect();
    let mut arr = [0u16; 32];
    let len = face.len().min(32);
    arr[..len].copy_from_slice(&face[..len]);
    CreateFontW(
        ncm.lfStatusFont.lfHeight, 0, 0, 0,
        600,
        0, 0, 0,
        1,
        0, 0, 0, 0,
        windows::core::PCWSTR::from_raw(arr.as_ptr()),
    )
}

unsafe fn panel_timer_font() -> HFONT {
    use windows::Win32::Graphics::Gdi::CreateFontW;
    let face: Vec<u16> = "Consolas\0".encode_utf16().collect();
    let mut arr = [0u16; 32];
    let len = face.len().min(32);
    arr[..len].copy_from_slice(&face[..len]);
    CreateFontW(
        -sc(28), 0, 0, 0,
        600, // FW_SEMIBOLD
        0, 0, 0,
        1, // DEFAULT_CHARSET
        0, 0, 0, 0,
        windows::core::PCWSTR::from_raw(arr.as_ptr()),
    )
}

unsafe fn panel_icon_font() -> HFONT {
    // Segoe MDL2 Assets — monochrome icon font on Windows 10+.
    // Renders glyphs as solid shapes that respect SetTextColor, unlike color emoji.
    use windows::Win32::Graphics::Gdi::CreateFontW;
    let face: Vec<u16> = "Segoe MDL2 Assets\0".encode_utf16().collect();
    let mut arr = [0u16; 32];
    let len = face.len().min(32);
    arr[..len].copy_from_slice(&face[..len]);
    CreateFontW(
        -sc(14), 0, 0, 0,
        400, 0, 0, 0,
        1, // DEFAULT_CHARSET
        0, 0, 0, 0,
        windows::core::PCWSTR::from_raw(arr.as_ptr()),
    )
}

unsafe extern "system" fn popup_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);

            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);

            let text = {
                let state = lock_state();
                state
                    .as_ref()
                    .map(|s| s.popup_text.clone())
                    .unwrap_or_default()
            };

            let br = CreateSolidBrush(COLORREF(0x00FFFFFF));
            FillRect(hdc, &rc, br);
            let _ = DeleteObject(br);
            FrameRect(hdc, &rc, GetSysColorBrush(COLOR_WINDOWFRAME));

            if !text.is_empty() {
                let hfont = tooltip_font();
                let old_font = SelectObject(hdc, hfont);
                let _ = SetBkMode(hdc, TRANSPARENT);
                let _ = SetTextColor(hdc, COLORREF(GetSysColor(COLOR_INFOTEXT)));
                let mut text_rc = RECT {
                    left: rc.left + POPUP_PAD_X,
                    top: rc.top + POPUP_PAD_Y,
                    right: rc.right - POPUP_PAD_X,
                    bottom: rc.bottom - POPUP_PAD_Y,
                };
                let mut wide: Vec<u16> = text.encode_utf16().collect();
                let _ = DrawTextW(
                    hdc,
                    &mut wide,
                    &mut text_rc,
                    DT_LEFT | DT_TOP | DT_WORDBREAK,
                );
                SelectObject(hdc, old_font);
                let _ = DeleteObject(hfont);
            }

            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn create_hover_popup(hinstance: windows::Win32::Foundation::HMODULE) -> Option<HWND> {
    unsafe {
        let class_name = native_interop::wide_str(POPUP_CLASS);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(popup_wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            ..Default::default()
        };
        RegisterClassExW(&wc);

        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            10,
            10,
            HWND::default(),
            HMENU::default(),
            HINSTANCE(hinstance.0),
            None,
        )
        .ok()
    }
}

unsafe extern "system" fn panel_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint_panel(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = loword(lparam.0 as u32) as i16 as i32;
            let y = hiword(lparam.0 as u32) as i16 as i32;
            update_panel_hot_button(hwnd, x, y);

            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            clear_panel_pointer_state(hwnd);
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if loword(wparam.0 as u32) == 0 {
                let (menu_open, pinned) = lock_state()
                    .as_ref()
                    .map(|s| (s.panel_menu_open, s.panel_pinned))
                    .unwrap_or((false, false));
                if !menu_open && !pinned {
                    hide_panel();
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_SETCURSOR => {
            // Show move cursor over the HTCAPTION drag zone when pinned
            let hit = (lparam.0 & 0xFFFF) as u16;
            if hit == 2 { // HTCAPTION
                let cursor = LoadCursorW(HINSTANCE::default(), IDC_SIZEALL).unwrap_or_default();
                SetCursor(cursor);
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_NCHITTEST => {
            let pinned = lock_state().as_ref().map(|s| s.panel_pinned).unwrap_or(false);
            if pinned {
                let mut pt = POINT {
                    x: (lparam.0 & 0xFFFF) as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                };
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let _ = ScreenToClient(hwnd, &mut pt);
                let hdr_bot = sc(PANEL_PAD) + sc(PANEL_HEADER_H);
                // Leave the button zone on the right as HTCLIENT so buttons still work
                let button_zone_w = sc(PANEL_BUTTON_W) * 3 + sc(4) * 2 + sc(PANEL_PAD);
                let drag_right = rc.right - button_zone_w;
                if pt.y >= 0 && pt.y < hdr_bot && pt.x >= 0 && pt.x < drag_right {
                    return LRESULT(2); // HTCAPTION — Windows handles the drag
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            let x = loword(lparam.0 as u32) as i16 as i32;
            let y = hiword(lparam.0 as u32) as i16 as i32;
            press_panel_button_at(hwnd, x, y);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            release_panel_button(hwnd);
            LRESULT(0)
        }
        WM_EXITSIZEMOVE => {
            // Fired when the user finishes dragging — save position to settings.
            let mut rc = RECT::default();
            let _ = GetWindowRect(hwnd, &mut rc);
            {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    if s.panel_pinned {
                        s.panel_pinned_x = Some(rc.left);
                        s.panel_pinned_y = Some(rc.top);
                    }
                }
            }
            save_state_settings();
            LRESULT(0)
        }
        WM_KEYDOWN => {
            match wparam.0 as u32 {
                0x1B => hide_panel(),                  // Escape
                0x09 => focus_next_panel_button(hwnd), // Tab
                _ => {}
            }
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            clear_panel_pointer_state(hwnd);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_PANEL_SHIMMER => {
            let has_data = {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    s.shimmer_phase = !s.shimmer_phase;
                    s.data.is_some()
                } else {
                    true
                }
            };
            if has_data {
                let _ = KillTimer(hwnd, TIMER_PANEL_SHIMMER);
            }
            let _ = InvalidateRect(hwnd, None, true);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn create_panel_window(hinstance: windows::Win32::Foundation::HMODULE) -> Option<HWND> {
    unsafe {
        let class_name = native_interop::wide_str(PANEL_CLASS);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(panel_wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let title = {
            let state = lock_state();
            let strings = state
                .as_ref()
                .map(|s| s.language.strings())
                .unwrap_or(LanguageId::English.strings());
            native_interop::wide_str(strings.window_title)
        };

        let (panel_w, panel_h) = current_panel_size();

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            panel_w,
            panel_h,
            HWND::default(),
            HMENU::default(),
            HINSTANCE(hinstance.0),
            None,
        )
        .ok()?;
        // Ask DWM to round window corners (Windows 11+) — smooth outer edges, no SetWindowRgn needed
        let pref = DWMWCP_ROUND;
        DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &pref as *const u32 as *const _, 4);
        Some(hwnd)
    }
}

fn current_panel_size() -> (i32, i32) {
    panel_size_for_models(active_model_count())
}

fn panel_size_for_models(active_models: i32) -> (i32, i32) {
    if active_models > 1 {
        (sc(PANEL_TWO_MODEL_W), sc(PANEL_TWO_MODEL_H))
    } else {
        (sc(PANEL_SINGLE_MODEL_W), sc(PANEL_SINGLE_MODEL_H))
    }
}

fn set_panel_title(hwnd: HWND, strings: Strings) {
    unsafe {
        let title = native_interop::wide_str(strings.window_title);
        let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(title.as_ptr()));
    }
}

fn hide_panel() {
    suppress_panel_reopen_for(Duration::from_millis(PANEL_REOPEN_SUPPRESS_MS));

    let (hwnd, was_pinned) = {
        let mut state = lock_state();
        let Some(s) = state.as_mut() else {
            return;
        };
        let was_pinned = s.panel_pinned;
        s.panel.visible = false;
        s.panel.hot_button = None;
        s.panel.pressed_button = None;
        s.panel_pinned = false;
        s.panel_pinned_x = None;
        s.panel_pinned_y = None;
        (s.panel.hwnd, was_pinned)
    };

    if was_pinned {
        save_state_settings();
    }

    if let Some(hwnd) = hwnd {
        unsafe {
            // Reset DWM backdrop and layering before hiding — an active acrylic/layered
            // backdrop persists on hidden windows and blocks input at the panel's position.
            let sbt = DWMSBT_AUTO;
            DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &sbt as *const u32 as *const _, 4);
            let ex = GetWindowLongW(hwnd, GWL_EXSTYLE);
            if ex & WS_EX_LAYERED.0 as i32 != 0 {
                SetWindowLongW(hwnd, GWL_EXSTYLE, ex & !(WS_EX_LAYERED.0 as i32));
            }
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

fn toggle_panel(widget_hwnd: HWND) {
    let should_hide = {
        let state = lock_state();
        state.as_ref().map(|s| s.panel.visible).unwrap_or(false)
    };

    if should_hide {
        hide_panel();
    } else if panel_reopen_suppressed() {
        return;
    } else {
        show_panel(widget_hwnd);
    }
}

fn suppress_panel_reopen_for(duration: Duration) {
    if let Ok(mut guard) = SUPPRESS_PANEL_REOPEN_UNTIL.lock() {
        *guard = Some(Instant::now() + duration);
    }
}

fn panel_reopen_suppressed() -> bool {
    SUPPRESS_PANEL_REOPEN_UNTIL
        .lock()
        .map(|guard| guard.is_some_and(|until| Instant::now() < until))
        .unwrap_or(false)
}

fn show_panel(widget_hwnd: HWND) {
    hide_hover_popup();

    let (panel_hwnd, panel_bg) = {
        let mut state = lock_state();
        let Some(s) = state.as_mut() else {
            return;
        };
        let Some(panel_hwnd) = s.panel.hwnd else {
            return;
        };
        s.panel.visible = true;
        s.panel.hot_button = None;
        s.panel.pressed_button = None;
        (panel_hwnd, s.panel_background)
    };

    position_panel_near_widget(widget_hwnd);

    unsafe {
        apply_panel_background(panel_hwnd, panel_bg);
        let _ = ShowWindow(panel_hwnd, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(panel_hwnd);
        let has_data = lock_state().as_ref().map(|s| s.data.is_some()).unwrap_or(true);
        if !has_data {
            SetTimer(panel_hwnd, TIMER_PANEL_SHIMMER, 600, None);
        }
    }
}

fn position_panel_near_widget(widget_hwnd: HWND) {
    let (panel_hwnd, pinned_pos) = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return;
        };
        let Some(panel_hwnd) = s.panel.hwnd else {
            return;
        };
        if !s.panel.visible {
            return;
        }
        let pinned_pos = if s.panel_pinned {
            Some((s.panel_pinned_x, s.panel_pinned_y))
        } else {
            None
        };
        (panel_hwnd, pinned_pos)
    };

    // Pinned with saved coords → restore to that position; pinned without → don't move
    if let Some((saved_x, saved_y)) = pinned_pos {
        if let (Some(x), Some(y)) = (saved_x, saved_y) {
            let (panel_w, panel_h) = current_panel_size();
            unsafe {
                let _ = SetWindowPos(panel_hwnd, HWND_TOPMOST, x, y, panel_w, panel_h, SWP_SHOWWINDOW);
            }
        }
        return;
    }

    let Some(widget_rect) = native_interop::get_window_rect_safe(widget_hwnd) else {
        return;
    };

    let (panel_w, panel_h) = current_panel_size();
    let gap = sc(8);

    unsafe {
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        let max_x = (screen_w - panel_w).max(0);
        let max_y = (screen_h - panel_h).max(0);

        let x = (widget_rect.right - panel_w).clamp(0, max_x);
        let above_y = widget_rect.top - panel_h - gap;
        let below_y = widget_rect.bottom + gap;
        let y = if above_y >= 0 {
            above_y
        } else {
            below_y.clamp(0, max_y)
        };

        let _ = SetWindowPos(
            panel_hwnd,
            HWND_TOPMOST,
            x,
            y,
            panel_w,
            panel_h,
            SWP_SHOWWINDOW,
        );
        // Outer corners handled by DWM (set at window creation via DwmSetWindowAttribute)
    }
}

fn refresh_panel_view(widget_hwnd: HWND) {
    let (panel_hwnd, visible) = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return;
        };
        (s.panel.hwnd, s.panel.visible)
    };

    if !visible {
        return;
    }

    position_panel_near_widget(widget_hwnd);
    if let Some(panel_hwnd) = panel_hwnd {
        unsafe {
            let _ = KillTimer(panel_hwnd, TIMER_PANEL_SHIMMER);
            let _ = InvalidateRect(panel_hwnd, None, true);
        }
    }
}

fn panel_button_hit_test(buttons: &[PanelButton], x: i32, y: i32) -> Option<PanelButtonId> {
    buttons
        .iter()
        .find(|button| button.enabled && point_in_rect(x, y, &button.rect))
        .map(|button| button.id)
}

fn point_in_rect(x: i32, y: i32, rect: &RECT) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

fn loword(value: u32) -> u16 {
    (value & 0xFFFF) as u16
}

fn hiword(value: u32) -> u16 {
    ((value >> 16) & 0xFFFF) as u16
}

fn update_panel_hot_button(hwnd: HWND, x: i32, y: i32) {
    let changed = {
        let mut state = lock_state();
        let Some(s) = state.as_mut() else {
            return;
        };
        let hot = panel_button_hit_test(&s.panel.buttons, x, y);
        if s.panel.hot_button != hot {
            s.panel.hot_button = hot;
            true
        } else {
            false
        }
    };
    if changed {
        update_panel_accessible_title(hwnd);
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
    }
}

fn press_panel_button_at(hwnd: HWND, x: i32, y: i32) {
    let changed = {
        let mut state = lock_state();
        let Some(s) = state.as_mut() else {
            return;
        };
        let pressed = panel_button_hit_test(&s.panel.buttons, x, y);
        s.panel.pressed_button = pressed;
        pressed.is_some()
    };
    if changed {
        unsafe {
            let _ = SetCapture(hwnd);
            let _ = InvalidateRect(hwnd, None, false);
        }
    }
}

fn release_panel_button(hwnd: HWND) {
    let action = {
        let mut state = lock_state();
        let Some(s) = state.as_mut() else {
            return;
        };
        let action = s
            .panel
            .pressed_button
            .filter(|pressed| s.panel.hot_button == Some(*pressed));
        s.panel.pressed_button = None;
        action
    };

    unsafe {
        let _ = ReleaseCapture();
        let _ = InvalidateRect(hwnd, None, false);
    }

    if let Some(action) = action {
        execute_panel_button(hwnd, action);
    }
}

fn execute_panel_button(panel_hwnd: HWND, action: PanelButtonId) {
    let widget_hwnd = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return;
        };
        s.hwnd.to_hwnd()
    };

    match action {
        PanelButtonId::Refresh => refresh_from_panel(widget_hwnd),
        PanelButtonId::Pin => {
            {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    s.panel_pinned = !s.panel_pinned;
                    if !s.panel_pinned {
                        s.panel_pinned_x = None;
                        s.panel_pinned_y = None;
                    }
                }
            }
            save_state_settings();
        }
        PanelButtonId::Settings => {
            {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() { s.panel_menu_open = true; }
            }
            show_context_menu(widget_hwnd); // blocks until menu is dismissed
            {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() { s.panel_menu_open = false; }
            }
            // Re-focus the panel so WM_ACTIVATE fires normally when user clicks away
            unsafe {
                if let Some(ph) = lock_state().as_ref().and_then(|s| s.panel.hwnd) {
                    let _ = SetForegroundWindow(ph);
                }
            }
            return;
        }
    }

    unsafe {
        let _ = InvalidateRect(panel_hwnd, None, true);
    }
}

fn refresh_from_panel(hwnd: HWND) {
    {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.session_text = "...".to_string();
            s.weekly_text = "...".to_string();
            s.force_notify_auth_error = true;
        }
    }
    render_layered();
    refresh_panel_view(hwnd);
    let sh = SendHwnd::from_hwnd(hwnd);
    std::thread::spawn(move || {
        do_poll(sh);
    });
}

fn clear_panel_pointer_state(hwnd: HWND) {
    let changed = {
        let mut state = lock_state();
        let Some(s) = state.as_mut() else {
            return;
        };
        let changed = s.panel.hot_button.is_some() || s.panel.pressed_button.is_some();
        s.panel.hot_button = None;
        s.panel.pressed_button = None;
        changed
    };
    if changed {
        update_panel_accessible_title(hwnd);
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
    }
}

fn update_panel_accessible_title(hwnd: HWND) {
    let title = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return;
        };
        let base = s.language.strings().window_title;
        let active_label = s.panel.hot_button.and_then(|id| {
            s.panel
                .buttons
                .iter()
                .find(|button| button.id == id)
                .map(|button| button.accessible_label.as_str())
        });
        match active_label {
            Some(label) => format!("{base} - {label}"),
            None => base.to_string(),
        }
    };

    unsafe {
        let title = native_interop::wide_str(&title);
        let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(title.as_ptr()));
    }
}

fn focus_next_panel_button(hwnd: HWND) {
    let changed = {
        let mut state = lock_state();
        let Some(s) = state.as_mut() else {
            return;
        };
        let enabled: Vec<PanelButtonId> = s
            .panel
            .buttons
            .iter()
            .filter(|button| button.enabled)
            .map(|button| button.id)
            .collect();
        if enabled.is_empty() {
            return;
        }

        let current = s
            .panel
            .hot_button
            .and_then(|id| enabled.iter().position(|candidate| *candidate == id));
        let next = current.map_or(0, |index| (index + 1) % enabled.len());
        s.panel.hot_button = Some(enabled[next]);
        true
    };
    if changed {
        update_panel_accessible_title(hwnd);
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
    }
}

fn panel_button_visual_state(panel: &PanelState, button: &PanelButton) -> PanelButtonVisualState {
    if !button.enabled {
        PanelButtonVisualState::Disabled
    } else if panel.pressed_button == Some(button.id) {
        PanelButtonVisualState::Pressed
    } else if panel.hot_button == Some(button.id) {
        PanelButtonVisualState::Hot
    } else {
        PanelButtonVisualState::Normal
    }
}

fn panel_buttons_for_layout(strings: Strings, width: i32, pinned: bool) -> Vec<PanelButton> {
    let right = width - sc(PANEL_PAD);
    let top = sc(PANEL_PAD) + (sc(PANEL_HEADER_H) - sc(PANEL_BUTTON_H)) / 2;
    let gap = sc(4);
    let refresh_left = right - sc(PANEL_BUTTON_W);
    let settings_right = refresh_left - gap;
    let settings_left = settings_right - sc(PANEL_BUTTON_W);
    let pin_right = settings_left - gap;
    let pin_left = pin_right - sc(PANEL_BUTTON_W);
    vec![
        PanelButton {
            id: PanelButtonId::Pin,
            rect: RECT { left: pin_left, top, right: pin_right, bottom: top + sc(PANEL_BUTTON_H) },
            label: "\u{E718}".to_string(), // Segoe MDL2 Assets: Pin
            accessible_label: "Pin".to_string(),
            enabled: true,
            selected: pinned,
        },
        PanelButton {
            id: PanelButtonId::Settings,
            rect: RECT { left: settings_left, top, right: settings_right, bottom: top + sc(PANEL_BUTTON_H) },
            label: "\u{E713}".to_string(), // Segoe MDL2 Assets: Settings
            accessible_label: strings.settings.to_string(),
            enabled: true,
            selected: false,
        },
        PanelButton {
            id: PanelButtonId::Refresh,
            rect: RECT { left: refresh_left, top, right, bottom: top + sc(PANEL_BUTTON_H) },
            label: "\u{E72C}".to_string(), // Segoe MDL2 Assets: Sync
            accessible_label: strings.refresh.to_string(),
            enabled: true,
            selected: false,
        },
    ]
}

fn panel_updated_text(strings: Strings, last_poll_at: Option<SystemTime>) -> String {
    let Some(last_poll_at) = last_poll_at else {
        return strings.panel_waiting_for_data.to_string();
    };
    let elapsed = SystemTime::now()
        .duration_since(last_poll_at)
        .unwrap_or_default();
    let secs = elapsed.as_secs();
    if secs < 60 {
        strings.now.to_string()
    } else if secs < 3600 {
        format!("{}{} ago", secs / 60, strings.minute_suffix)
    } else {
        format!("{}{} ago", secs / 3600, strings.hour_suffix)
    }
}

fn panel_header_status_text(
    strings: Strings,
    last_poll_at: Option<SystemTime>,
    last_poll_ok: bool,
    last_poll_status: Option<UsagePollStatus>,
) -> String {
    let updated = panel_updated_text(strings, last_poll_at);
    if last_poll_ok {
        return updated;
    }

    match last_poll_status {
        Some(UsagePollStatus::NoCredentials) => strings.panel_error_missing_credentials.to_string(),
        Some(UsagePollStatus::AuthRequired | UsagePollStatus::TokenExpired) => {
            strings.panel_error_token_expired.to_string()
        }
        Some(UsagePollStatus::RequestFailed) => {
            format!(
                "{} - {} - {}",
                strings.panel_retrying, strings.panel_last_known_data, updated
            )
        }
        _ => updated,
    }
}

fn panel_model_sections(state: &AppState) -> Vec<PanelModelSection> {
    let mut sections = Vec::new();
    let strings = state.language.strings();
    let claude_usage = state
        .data
        .as_ref()
        .and_then(|data| data.claude_code.as_ref());
    let base_issue = panel_issue_from_poll_status(state.last_poll_status);
    if state.show_claude_code {
        let issue = panel_model_issue(base_issue, state.last_poll_ok, claude_usage.is_some());
        let session_pct = claude_usage.map(|u| u.session.percentage);
        sections.push(PanelModelSection {
            name: strings.claude_code_model,
            session: panel_usage_window(
                session_pct,
                claude_usage.and_then(|usage| usage.session.resets_at),
            ),
            weekly: panel_usage_window(
                claude_usage.map(|usage| usage.weekly.percentage),
                claude_usage.and_then(|usage| usage.weekly.resets_at),
            ),
            session_state: state_from_utilization(session_pct),
            issue,
            user_label: claude_usage.and_then(|u| u.user_label.clone()),
            message_count: claude_usage.and_then(|u| u.session.message_count),
            token_count: claude_usage.and_then(|u| u.session.token_count),
            email: claude_usage.and_then(|u| u.email.clone()),
        });
    }
    sections
}

fn panel_issue_from_poll_status(status: Option<UsagePollStatus>) -> Option<PanelIssue> {
    match status {
        Some(UsagePollStatus::NoCredentials) => Some(PanelIssue::MissingCredentials),
        Some(UsagePollStatus::AuthRequired | UsagePollStatus::TokenExpired) => {
            Some(PanelIssue::TokenExpired)
        }
        Some(UsagePollStatus::RequestFailed) => Some(PanelIssue::Network),
        _ => None,
    }
}

fn panel_model_issue(
    base_issue: Option<PanelIssue>,
    last_poll_ok: bool,
    model_has_data: bool,
) -> Option<PanelIssue> {
    if !last_poll_ok {
        base_issue
    } else if !model_has_data {
        Some(PanelIssue::Partial)
    } else {
        None
    }
}


fn panel_usage_window(
    percentage: Option<f64>,
    resets_at: Option<SystemTime>,
) -> PanelUsageWindow {
    PanelUsageWindow {
        percentage,
        reset_time: native_interop::format_local_reset_time(resets_at, true),
        resets_at,
        status: panel_usage_status(percentage),
    }
}

fn panel_usage_status(percentage: Option<f64>) -> PanelUsageStatus {
    let Some(percentage) = percentage else {
        return PanelUsageStatus::Unknown;
    };
    if percentage >= 100.0 {
        PanelUsageStatus::AtLimit
    } else if percentage >= 80.0 {
        PanelUsageStatus::NearLimit
    } else if percentage >= 50.0 {
        PanelUsageStatus::Caution
    } else {
        PanelUsageStatus::Normal
    }
}


fn paint_panel(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);

        let width = rc.right - rc.left;
        let height = rc.bottom - rc.top;
        if width <= 0 || height <= 0 {
            let _ = EndPaint(hwnd, &ps);
            return;
        }

        let (is_dark, strings, updated_text, header_badge, model_sections, panel_snapshot, shimmer_phase) = {
            let mut state = lock_state();
            match state.as_mut() {
                Some(s) => {
                    let strings = s.language.strings();
                    s.panel.buttons = panel_buttons_for_layout(strings, width, s.panel_pinned);
                    let badge = s.data.as_ref()
                        .and_then(|d| d.claude_code.as_ref())
                        .and_then(|u| u.user_label.clone())
                        .unwrap_or_else(|| "PRO".to_string());
                    (
                        s.is_dark,
                        strings,
                        panel_header_status_text(
                            strings,
                            s.last_poll_at,
                            s.last_poll_ok,
                            s.last_poll_status,
                        ),
                        badge,
                        panel_model_sections(s),
                        s.panel.clone(),
                        s.shimmer_phase,
                    )
                }
                None => (
                    theme::is_dark_mode(),
                    LanguageId::English.strings(),
                    LanguageId::English.strings().panel_waiting_for_data.to_string(),
                    "PRO".to_string(),
                    Vec::new(),
                    PanelState::default(),
                    false,
                ),
            }
        };

        let colors = PanelColors::new(is_dark);
        let bg = colors.bg;
        let border = colors.border;
        let muted = colors.muted;

        // Background fill — always draw for Solid/Translucent
        let bg_brush = CreateSolidBrush(COLORREF(bg.to_colorref()));
        let _ = FillRect(hdc, &rc, bg_brush);
        let _ = DeleteObject(bg_brush);

        let font = tooltip_font();
        let old_font = SelectObject(hdc, font);
        let _ = SetBkMode(hdc, TRANSPARENT);

        // Header strip: "Claude Code [PRO] · 1m ago" left, buttons right
        let hdr_top = sc(PANEL_PAD);
        let hdr_bot = hdr_top + sc(PANEL_HEADER_H);
        let hdr_left = sc(14);
        // Reserve space for pin + settings + refresh buttons on the right
        let buttons_w = sc(PANEL_BUTTON_W) * 3 + sc(4) * 2 + sc(PANEL_PAD);
        let hdr_right = width - buttons_w;

        // "Claude Code" — primary text, weight 600 (system bold)
        let product = strings.claude_code_model;
        let product_w = measure_text_width(hdc, product);
        let mut product_rect = RECT { left: hdr_left, top: hdr_top, right: hdr_left + product_w, bottom: hdr_bot };
        draw_panel_text(hdc, product, &mut product_rect, &colors.text, DT_LEFT | DT_VCENTER | DT_SINGLELINE);

        // PRO badge pill — measured and drawn in bold
        let badge_text = header_badge.to_uppercase();
        let badge_pad = sc(10);
        let badge_bold_font = panel_bold_font();
        let old_badge_font = SelectObject(hdc, badge_bold_font);
        let badge_w = measure_text_width(hdc, &badge_text) + badge_pad * 2;
        let badge_gap = sc(6);
        let pill_cx = hdr_left + product_w + badge_gap;
        let pill_cy_top = hdr_top + (sc(PANEL_HEADER_H) - sc(16)) / 2;
        let pill_cy_bot = pill_cy_top + sc(16);
        let pill_h = pill_cy_bot - pill_cy_top;
        gdip_stroke_rounded(hdc, pill_cx, pill_cy_top, pill_cx + badge_w, pill_cy_bot, pill_h / 2, colors.badge_border, 2.0);
        let mut badge_rect = RECT { left: pill_cx, top: pill_cy_top, right: pill_cx + badge_w, bottom: pill_cy_bot };
        draw_panel_text(hdc, &badge_text, &mut badge_rect, &colors.badge_text, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        SelectObject(hdc, old_badge_font);
        let _ = DeleteObject(badge_bold_font);

        // " · " separator
        let sep = " \u{00B7} ";
        let sep_w = measure_text_width(hdc, sep);
        let sep_x = pill_cx + badge_w;
        let mut sep_rect = RECT { left: sep_x, top: hdr_top, right: sep_x + sep_w, bottom: hdr_bot };
        draw_panel_text(hdc, sep, &mut sep_rect, &colors.dim, DT_LEFT | DT_VCENTER | DT_SINGLELINE);

        // Timestamp — muted, truncate if needed
        let ts_x = sep_x + sep_w;
        let mut ts_rect = RECT { left: ts_x, top: hdr_top, right: hdr_right, bottom: hdr_bot };
        draw_panel_text(hdc, &updated_text, &mut ts_rect, &muted, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);

        // Divider below header
        draw_panel_separator(hdc, 0, hdr_bot, width, &colors.divider);

        let content_top = hdr_bot + sc(1);
        draw_panel_model_sections(
            hdc,
            &model_sections,
            RECT {
                left: 0,
                top: content_top,
                right: width,
                bottom: height,
            },
            strings,
            &colors,
            shimmer_phase,
        );

        for button in &panel_snapshot.buttons {
            draw_panel_button(
                hdc,
                button,
                panel_button_visual_state(&panel_snapshot, button),
                &colors,
            );
        }

        // Border drawn last — anti-aliased rounded stroke matching DWM corner radius
        let panel_radius = sc(PANEL_CORNER_RADIUS);
        gdip_stroke_rounded(hdc, rc.left, rc.top, rc.right - 1, rc.bottom - 1, panel_radius, border, 1.0);

        SelectObject(hdc, old_font);
        let _ = DeleteObject(font);
        let _ = EndPaint(hwnd, &ps);
    }
}

unsafe fn draw_panel_weekly(
    hdc: HDC,
    section: &PanelModelSection,
    rect: RECT,
    strings: Strings,
    colors: &PanelColors,
) {
    let left_x  = rect.left + sc(14);
    let right_x = rect.right - sc(14);
    let row_top = rect.top + sc(12);

    // System font metrics (tooltip_font is selected on entry)
    let mut tm = TEXTMETRICW::default();
    let _ = GetTextMetricsW(hdc, &mut tm);
    let cap_h    = tm.tmAscent - tm.tmInternalLeading;
    let row_cy   = row_top + tm.tmHeight / 2;
    let baseline = row_cy + cap_h / 2;

    let prev_align = SetTextAlign(hdc, TA_LEFT | TA_BASELINE);

    // ── Left: "WEEKLY" bold muted ──────────────────────────────────────────
    let bold_font = panel_bold_font();
    let old_bold  = SelectObject(hdc, bold_font);
    let mut tm_bold = TEXTMETRICW::default();
    let _ = GetTextMetricsW(hdc, &mut tm_bold);
    let bold_baseline = row_cy + (tm_bold.tmAscent - tm_bold.tmInternalLeading) / 2;
    let _ = SetTextColor(hdc, COLORREF(colors.muted.to_colorref()));
    let label_wide: Vec<u16> = strings.panel_weekly_label.encode_utf16().collect();
    let _ = TextOutW(hdc, left_x, bold_baseline, &label_wide);
    let label_w = measure_text_width(hdc, strings.panel_weekly_label);
    SelectObject(hdc, old_bold);
    let _ = DeleteObject(bold_font);

    // " resets Thu 11:00 am" dim, system font
    let resets_str = if section.weekly.reset_time.is_empty() {
        format!(" {}", strings.panel_resets)
    } else {
        format!(" {} {}", strings.panel_resets, section.weekly.reset_time)
    };
    let _ = SetTextColor(hdc, COLORREF(colors.dim.to_colorref()));
    let resets_wide: Vec<u16> = resets_str.encode_utf16().collect();
    let _ = TextOutW(hdc, left_x + label_w, baseline, &resets_wide);

    // ── Right: "{pct}%" mono green + " {countdown} left" dim ──────────────
    let countdown = poller::format_countdown(section.weekly.resets_at, strings, true);
    let suffix = if countdown.is_empty() {
        " -- left".to_string()
    } else {
        format!(" {} left", countdown)
    };
    let suffix_w = measure_text_width(hdc, &suffix);

    // "43%" in a single bold font so number and % render at the same optical size
    let pct_str = match section.weekly.percentage {
        Some(p) => format!("{:.0}%", p.round()),
        None    => "--".to_string(),
    };
    let bold_pct_font = panel_bold_font();
    let old_bold_pct  = SelectObject(hdc, bold_pct_font);
    let mut tm_bold_pct = TEXTMETRICW::default();
    let _ = GetTextMetricsW(hdc, &mut tm_bold_pct);
    let bold_pct_baseline = row_cy + (tm_bold_pct.tmAscent - tm_bold_pct.tmInternalLeading) / 2;
    let pct_w = measure_text_width(hdc, &pct_str);
    let pct_x = right_x - suffix_w - pct_w;
    let _ = SetTextColor(hdc, COLORREF(colors.green.to_colorref()));
    let pct_wide: Vec<u16> = pct_str.encode_utf16().collect();
    let _ = TextOutW(hdc, pct_x, bold_pct_baseline, &pct_wide);
    SelectObject(hdc, old_bold_pct);
    let _ = DeleteObject(bold_pct_font);

    // Suffix dim, system font
    let _ = SetTextColor(hdc, COLORREF(colors.dim.to_colorref()));
    let suffix_wide: Vec<u16> = suffix.encode_utf16().collect();
    let _ = TextOutW(hdc, pct_x + pct_w, baseline, &suffix_wide);

    let _ = SetTextAlign(hdc, TEXT_ALIGN_OPTIONS(prev_align));

    // ── Progress bar ──────────────────────────────────────────────────────
    let bar_top = row_top + tm.tmHeight + sc(10);
    let bar_bot = bar_top + sc(6);
    let bar_r   = (bar_bot - bar_top) / 2; // pill: radius = half height

    gdip_fill_rounded(hdc, left_x, bar_top, right_x, bar_bot, bar_r, colors.green_soft);

    let fill_pct = (section.weekly.percentage.unwrap_or(0.0) / 100.0).clamp(0.0, 1.0);
    let fill_w   = ((right_x - left_x) as f64 * fill_pct).round() as i32;
    if fill_w > bar_r {
        gdip_fill_rounded(hdc, left_x, bar_top, left_x + fill_w, bar_bot, bar_r, colors.green);
    }
}

unsafe fn draw_panel_separator(hdc: HDC, left: i32, y: i32, right: i32, color: &Color) {
    let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
    let rect = RECT {
        left,
        top: y,
        right,
        bottom: y + 1,
    };
    FillRect(hdc, &rect, brush);
    let _ = DeleteObject(brush);
}

unsafe fn draw_panel_model_sections(
    hdc: HDC,
    sections: &[PanelModelSection],
    bounds: RECT,
    strings: Strings,
    colors: &PanelColors,
    shimmer_phase: bool,
) {
    if sections.is_empty() {
        let mut rect = bounds;
        draw_panel_text(
            hdc,
            strings.panel_waiting_for_data,
            &mut rect,
            &colors.muted,
            DT_LEFT | DT_TOP | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        return;
    }

    let mut top = bounds.top;
    for (i, section) in sections.iter().enumerate() {
        draw_panel_model_section(
            hdc,
            section,
            RECT {
                left: bounds.left,
                top,
                right: bounds.right,
                bottom: top + sc(PANEL_CARD_H),
            },
            strings,
            colors,
            shimmer_phase,
        );
        top += sc(PANEL_CARD_H);
        if i + 1 < sections.len() {
            draw_panel_separator(hdc, bounds.left, top + sc(4), bounds.right, &colors.border);
            top += sc(14);
        }
    }
}

unsafe fn draw_panel_model_section(
    hdc: HDC,
    section: &PanelModelSection,
    rect: RECT,
    strings: Strings,
    colors: &PanelColors,
    shimmer_phase: bool,
) {
    let state = section.session_state;
    let hero_top = rect.top;
    let hero_bot = hero_top + sc(PANEL_HERO_H);
    let weekly_top = hero_bot;
    let weekly_bot = weekly_top + sc(PANEL_WEEKLY_H);
    let _ = weekly_bot;

    // ── Hero block ───────────────────────────────────────────────────────────
    // Gradient wash: state.X-soft at top → panel.bg at bottom
    let soft = colors.session_soft(state);
    let bg   = colors.bg;
    let verts = [
        TRIVERTEX {
            x: rect.left,
            y: hero_top,
            Red:   (soft.r as u16) << 8,
            Green: (soft.g as u16) << 8,
            Blue:  (soft.b as u16) << 8,
            Alpha: 0,
        },
        TRIVERTEX {
            x: rect.right,
            y: hero_bot,
            Red:   (bg.r as u16) << 8,
            Green: (bg.g as u16) << 8,
            Blue:  (bg.b as u16) << 8,
            Alpha: 0,
        },
    ];
    let mesh = GRADIENT_RECT { UpperLeft: 0, LowerRight: 1 };
    let _ = GradientFill(hdc, &verts, &mesh as *const GRADIENT_RECT as *const _, 1, GRADIENT_FILL_RECT_V);

    // ── Status chip row ──────────────────────────────────────────────────────
    let accent_full = colors.session_accent(state);
    // Capped dot pulse: blend accent toward soft background at ~55% opacity when dim phase
    let accent = if state == SessionState::Capped && !shimmer_phase {
        let a = 140u16; // ~55% of 255
        let b = 255u16 - a;
        Color {
            r: ((accent_full.r as u16 * a + soft.r as u16 * b) / 255) as u8,
            g: ((accent_full.g as u16 * a + soft.g as u16 * b) / 255) as u8,
            b: ((accent_full.b as u16 * a + soft.b as u16 * b) / 255) as u8,
        }
    } else {
        accent_full
    };

    // Select bold font first so GetTextMetrics reflects the actual font used for the label
    let bold_font = panel_bold_font();
    let old_font = SelectObject(hdc, bold_font);
    let mut tm = TEXTMETRICW::default();
    let _ = GetTextMetricsW(hdc, &mut tm);

    // chip row: 18px top-padding, height = max(glow height, font height)
    let dot_sz  = sc(7);   // spec: 7×7px filled circle
    let glow_sz = sc(11);  // spec: 11×11px glow behind dot
    let row_h   = glow_sz.max(tm.tmHeight);
    let chip_top_y = hero_top + sc(18);
    let chip_cy    = chip_top_y + row_h / 2; // shared vertical center for dot and text

    let dot_left = rect.left + sc(14);
    let dot_cx   = dot_left + dot_sz / 2;

    // Glow halo: accent blended at ~25% over soft background
    let glow = Color {
        r: (accent.r as u16 * 25 / 100 + soft.r as u16 * 75 / 100) as u8,
        g: (accent.g as u16 * 25 / 100 + soft.g as u16 * 75 / 100) as u8,
        b: (accent.b as u16 * 25 / 100 + soft.b as u16 * 75 / 100) as u8,
    };
    gdip_fill_ellipse(hdc,
        dot_cx - glow_sz / 2, chip_cy - glow_sz / 2,
        dot_cx - glow_sz / 2 + glow_sz, chip_cy - glow_sz / 2 + glow_sz,
        glow,
    );
    gdip_fill_ellipse(hdc,
        dot_left, chip_cy - dot_sz / 2,
        dot_left + dot_sz, chip_cy - dot_sz / 2 + dot_sz,
        accent,
    );

    // Status label — use TA_BASELINE so the text baseline is set precisely.
    // baseline = chip_cy + cap_height/2 centers capital letters at chip_cy.
    let chip_label = match state {
        SessionState::Capped      => "SESSION AT CAP",
        SessionState::SlowingDown => "SLOWING DOWN",
        SessionState::GoingSteady => "GOING STEADY",
        SessionState::PlentyLeft  => "PLENTY LEFT",
    };
    let label_x = dot_left + dot_sz + sc(7);
    let cap_height = tm.tmAscent - tm.tmInternalLeading;
    let baseline_y = chip_cy + cap_height / 2;
    let _ = SetTextColor(hdc, COLORREF(accent.to_colorref()));
    let prev_align = SetTextAlign(hdc, TA_LEFT | TA_BASELINE);
    let wide: Vec<u16> = chip_label.encode_utf16().collect();
    let _ = TextOutW(hdc, label_x, baseline_y, &wide);
    let _ = SetTextAlign(hdc, TEXT_ALIGN_OPTIONS(prev_align));
    SelectObject(hdc, old_font);
    let _ = DeleteObject(bold_font);

    // Email — right-aligned on chip row, dim color, normal-weight system font
    if let Some(ref email) = section.email {
        let email_font = panel_small_font();
        let old_email_font = SelectObject(hdc, email_font);
        let _ = SetTextColor(hdc, COLORREF(colors.dim.to_colorref()));
        let prev_align_email = SetTextAlign(hdc, TA_RIGHT | TA_BASELINE);
        let email_wide: Vec<u16> = email.encode_utf16().collect();
        let _ = TextOutW(hdc, rect.right - sc(14), baseline_y, &email_wide);
        let _ = SetTextAlign(hdc, TEXT_ALIGN_OPTIONS(prev_align_email));
        SelectObject(hdc, old_email_font);
        let _ = DeleteObject(email_font);
    }

    // ── Timer row ─────────────────────────────────────────────────────────────
    // Positioned below the chip row (18px top-pad + row_h + 10px gap)
    let timer_raw = poller::format_countdown(section.session.resets_at, strings, true);
    let timer_display = if timer_raw.is_empty() { "--".to_string() } else { timer_raw };
    let timer_x = rect.left + sc(14);
    let timer_top = chip_top_y + row_h + sc(10);

    // Big Consolas numerals: sc(28) height, weight 600, colors.text
    let timer_font = panel_timer_font();
    let old_timer_font = SelectObject(hdc, timer_font);
    let mut tm_timer = TEXTMETRICW::default();
    let _ = GetTextMetricsW(hdc, &mut tm_timer);
    let timer_baseline = timer_top + tm_timer.tmAscent;
    let timer_w = measure_text_width(hdc, &timer_display);

    let prev_align2 = SetTextAlign(hdc, TA_LEFT | TA_BASELINE);
    let _ = SetTextColor(hdc, COLORREF(colors.text.to_colorref()));
    let wide_timer: Vec<u16> = timer_display.encode_utf16().collect();
    let _ = TextOutW(hdc, timer_x, timer_baseline, &wide_timer);

    // Restore system font for the helper text
    SelectObject(hdc, old_timer_font);
    let _ = DeleteObject(timer_font);

    // Helper text on the same baseline — system font, colors.muted
    let helper = match state {
        SessionState::Capped | SessionState::SlowingDown => strings.panel_timer_until_reset,
        SessionState::GoingSteady | SessionState::PlentyLeft => strings.panel_timer_left_session,
    };
    let helper_x = timer_x + timer_w + sc(10);
    let _ = SetTextColor(hdc, COLORREF(colors.muted.to_colorref()));
    let wide_helper: Vec<u16> = helper.encode_utf16().collect();
    let _ = TextOutW(hdc, helper_x, timer_baseline, &wide_helper);
    let _ = SetTextAlign(hdc, TEXT_ALIGN_OPTIONS(prev_align2));

    // ── Detail line ───────────────────────────────────────────────────────────
    // 6px below the timer row; system font (tooltip_font still selected), colors.dim
    let detail_y = timer_top + tm_timer.tmHeight + sc(6);
    let reset_part = if section.session.reset_time.is_empty() {
        "--".to_string()
    } else {
        section.session.reset_time.clone()
    };
    let msgs_str   = section.message_count.map(|n| format_with_commas(n as u64)).unwrap_or_else(|| "--".to_string());
    let tokens_str = section.token_count.map(format_with_commas).unwrap_or_else(|| "--".to_string());
    let detail_text = format!("Resets at {reset_part} \u{00B7} {msgs_str} msgs \u{00B7} {tokens_str} tokens");
    let mut detail_rect = RECT {
        left:   rect.left + sc(14),
        top:    detail_y,
        right:  rect.right - sc(14),
        bottom: detail_y + sc(20),
    };
    draw_panel_text(hdc, &detail_text, &mut detail_rect, &colors.dim,
        DT_LEFT | DT_TOP | DT_SINGLELINE | DT_END_ELLIPSIS);

    // Hero block bottom divider
    draw_panel_separator(hdc, rect.left, hero_bot, rect.right, &colors.divider);

    // ── Weekly block ─────────────────────────────────────────────────────────
    draw_panel_weekly(hdc, section, RECT { left: rect.left, top: weekly_top, right: rect.right, bottom: weekly_bot }, strings, colors);

    // Weekly block bottom divider
    draw_panel_separator(hdc, rect.left, weekly_bot, rect.right, &colors.divider);
}

fn format_with_commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 { out.push(','); }
        out.push(ch);
    }
    out.chars().rev().collect()
}

unsafe fn measure_text_width(hdc: HDC, text: &str) -> i32 {
    let wide: Vec<u16> = text.encode_utf16().collect();
    let mut size = SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &wide, &mut size);
    size.cx
}

#[allow(dead_code)]
unsafe fn draw_skeleton_bone(hdc: HDC, left: i32, top: i32, right: i32, bottom: i32, bright: bool, colors: &PanelColors) {
    let base = colors.divider;
    let color = if bright {
        Color { r: base.r.saturating_add(20), g: base.g.saturating_add(20), b: base.b.saturating_add(20) }
    } else {
        base
    };
    let r = (bottom - top).max(1);
    let null_pen = CreatePen(PS_NULL, 0, COLORREF(0));
    let old_pen = SelectObject(hdc, null_pen);
    let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
    let old_brush = SelectObject(hdc, brush);
    let _ = RoundRect(hdc, left, top, right, bottom, r, r);
    SelectObject(hdc, old_brush);
    let _ = DeleteObject(brush);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(null_pen);
}

#[allow(dead_code)]
unsafe fn draw_skeleton_rows(hdc: HDC, cl: i32, cr: i32, card_top: i32, bright: bool, colors: &PanelColors) {
    let pct_w = sc(52);
    let gap = sc(8);
    let left_w = cr - cl - pct_w - gap;

    for row_top in [card_top + sc(34), card_top + sc(72)] {
        draw_skeleton_bone(hdc, cl, row_top,           cl + left_w - sc(20), row_top + sc(12), bright, colors);
        draw_skeleton_bone(hdc, cl, row_top + sc(15),  cl + left_w - sc(50), row_top + sc(25), !bright, colors);
        draw_skeleton_bone(hdc, cr - pct_w + sc(6),   row_top, cr, row_top + sc(24), bright, colors);
    }
}


unsafe fn draw_panel_text(
    hdc: HDC,
    value: &str,
    rect: &mut RECT,
    color: &Color,
    format: DRAW_TEXT_FORMAT,
) {
    let _ = SetTextColor(hdc, COLORREF(color.to_colorref()));
    let mut wide: Vec<u16> = value.encode_utf16().collect();
    let _ = DrawTextW(hdc, &mut wide, rect, format);
}

unsafe fn draw_panel_button(
    hdc: HDC,
    button: &PanelButton,
    state: PanelButtonVisualState,
    colors: &PanelColors,
) {
    let accent = claude_accent_color();
    // Icon color: accent when selected (pinned), full text on hover/press, muted at rest
    let icon_color = if button.selected {
        accent
    } else {
        match state {
            PanelButtonVisualState::Normal   => colors.muted,
            PanelButtonVisualState::Hot      => colors.text,
            PanelButtonVisualState::Pressed  => colors.text,
            PanelButtonVisualState::Disabled => colors.dim,
        }
    };

    // Background: nothing at rest, subtle rounded fill on hover/press — no border ever
    let btn_r = sc(4);
    match state {
        PanelButtonVisualState::Hot | PanelButtonVisualState::Pressed => {
            gdip_fill_rounded(hdc, button.rect.left, button.rect.top, button.rect.right, button.rect.bottom, btn_r, colors.bg_soft);
        }
        _ => {}
    }

    let icon_font = panel_icon_font();
    let prev_font = SelectObject(hdc, icon_font);
    let mut text_rect = button.rect;
    draw_panel_text(hdc, &button.label, &mut text_rect, &icon_color, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    SelectObject(hdc, prev_font);
    let _ = DeleteObject(icon_font);
}

fn reset_tooltip_suffix(session: Option<SystemTime>, weekly: Option<SystemTime>) -> String {
    let t1 = native_interop::format_local_reset_time(session, false);
    let t2 = native_interop::format_local_reset_time(weekly, false);
    if t1.is_empty() && t2.is_empty() {
        String::new()
    } else {
        format!("\nReset: 5h {t1} / 7d {t2}")
    }
}

fn build_popup_text(s: &AppState) -> String {
    if !s.last_poll_ok {
        return String::new();
    }
    let Some(data) = s.data.as_ref() else {
        return String::new();
    };
    let mut lines = Vec::new();
    let mut push =
        |show: bool, name: &str, session: Option<SystemTime>, weekly: Option<SystemTime>| {
            if !show {
                return;
            }
            let t1 = native_interop::format_local_reset_time(session, false);
            let t2 = native_interop::format_local_reset_time(weekly, false);
            if !t1.is_empty() || !t2.is_empty() {
                lines.push(format!("{name} — 5h resets {t1}  ·  7d resets {t2}"));
            }
        };
    if let Some(cc) = data.claude_code.as_ref() {
        push(
            s.show_claude_code,
            "Claude Code",
            cc.session.resets_at,
            cc.weekly.resets_at,
        );
    }
    lines.join("\n")
}

fn show_hover_popup(widget_hwnd: HWND) {
    let (popup_hwnd, text) = {
        let mut state = lock_state();
        let Some(s) = state.as_mut() else { return };
        let text = build_popup_text(s);
        s.popup_text = text.clone();
        (s.popup_hwnd, text)
    };
    let Some(popup) = popup_hwnd else { return };
    if text.is_empty() {
        return;
    }

    let (text_w, text_h) = unsafe {
        let screen_dc = GetDC(HWND::default());
        let hfont = tooltip_font();
        let old_font = SelectObject(screen_dc, hfont);
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        let mut measure_rc = RECT {
            left: 0,
            top: 0,
            right: sc(500),
            bottom: sc(200),
        };
        let _ = DrawTextW(
            screen_dc,
            &mut wide,
            &mut measure_rc,
            DT_LEFT | DT_TOP | DT_WORDBREAK | DT_CALCRECT,
        );
        SelectObject(screen_dc, old_font);
        let _ = DeleteObject(hfont);
        ReleaseDC(HWND::default(), screen_dc);
        (
            measure_rc.right - measure_rc.left,
            measure_rc.bottom - measure_rc.top,
        )
    };

    let Some(widget_rect) = native_interop::get_window_rect_safe(widget_hwnd) else {
        return;
    };
    let popup_w = text_w + POPUP_PAD_X * 2;
    let popup_h = text_h + POPUP_PAD_Y * 2;
    let x = (widget_rect.right - popup_w).max(0);
    let y = widget_rect.top - popup_h - sc(4);

    unsafe {
        let _ = SetWindowPos(
            popup,
            HWND_TOPMOST,
            x,
            y,
            popup_w,
            popup_h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        let _ = InvalidateRect(popup, None, true);
    }
}

fn hide_hover_popup() {
    let popup = {
        let state = lock_state();
        state.as_ref().and_then(|s| s.popup_hwnd)
    };
    if let Some(popup) = popup {
        unsafe {
            let _ = ShowWindow(popup, SW_HIDE);
        }
    }
}

pub fn run() {
    // Enable Per-Monitor DPI Awareness V2 for crisp rendering at any scale factor
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CURRENT_DPI.store(GetDpiForSystem(), Ordering::Relaxed);
    }
    init_gdiplus();
    diagnose::log("window::run started");

    // Single-instance guard: silently exit if another instance is running
    let mutex_name = native_interop::wide_str("Global\\ClaudeCodeUsageMonitor");
    let _mutex = unsafe {
        let handle = CreateMutexW(None, false, PCWSTR::from_raw(mutex_name.as_ptr()));
        match handle {
            Ok(h) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    diagnose::log("startup aborted: another instance is already running");
                    return;
                }
                h
            }
            Err(error) => {
                diagnose::log_error(
                    "startup aborted: unable to create single-instance mutex",
                    error,
                );
                return;
            }
        }
    };

    let class_name = native_interop::wide_str("ClaudeCodeUsageMonitor");

    unsafe {
        let hinstance = GetModuleHandleW(PCWSTR::null()).unwrap();
        let (large_icon, small_icon) = load_embedded_app_icons();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hIcon: large_icon,
            hIconSm: small_icon,
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            ..Default::default()
        };

        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            diagnose::log("RegisterClassExW returned 0");
        }

        let settings = load_settings();
        let language_override = settings.language.as_deref().and_then(LanguageId::from_code);
        let language = localization::resolve_language(language_override);
        let install_channel = updater::current_install_channel();

        // Create as layered popup (will be reparented into taskbar)
        let title = native_interop::wide_str(language.strings().window_title);
        let initial_model_count = active_model_count();
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            total_widget_width_for(initial_model_count),
            sc(WIDGET_HEIGHT),
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        )
        .unwrap();

        if !large_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_BIG as usize),
                LPARAM(large_icon.0 as isize),
            );
        }
        if !small_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_SMALL as usize),
                LPARAM(small_icon.0 as isize),
            );
        }

        diagnose::log(format!("main window created hwnd={:?}", hwnd));

        let is_dark = theme::is_dark_mode();
        let mut embedded = false;

        {
            let mut state = lock_state();
            *state = Some(AppState {
                hwnd: SendHwnd::from_hwnd(hwnd),
                taskbar_hwnd: None,
                tray_notify_hwnd: None,
                win_event_hook: None,
                is_dark,
                embedded: false,
                language_override,
                language,
                install_channel,
                session_percent: 0.0,
                session_text: "--".to_string(),
                session_resets_at: None,
                weekly_percent: 0.0,
                weekly_text: "--".to_string(),
                show_claude_code: settings.show_claude_code,
                data: None,
                poll_interval_ms: settings.poll_interval_ms,
                compound_countdown: settings.compound_countdown,
                retry_count: 0,
                force_notify_auth_error: false,
                auth_error_paused_polling: false,
                auth_watch_mode: poller::CredentialWatchMode::ActiveSource,
                auth_watch_snapshot: Vec::new(),
                last_poll_ok: false,
                last_poll_at: None,
                last_poll_status: None,
                update_status: UpdateStatus::Idle,
                last_update_check_unix: settings.last_update_check_unix,
                tray_offset: settings.tray_offset,
                dragging: false,
                drag_start_mouse_x: 0,
                drag_start_offset: 0,
                widget_visible: settings.widget_visible,
                panel_background: settings.panel_background,
                panel_pinned: settings.panel_pinned,
                panel_pinned_x: settings.panel_pinned_x,
                panel_pinned_y: settings.panel_pinned_y,
                panel_menu_open: false,
                popup_hwnd: None,
                popup_text: String::new(),
                mouse_over_widget: false,
                panel: PanelState::default(),
                shimmer_phase: false,
            });
        }

        // Try to embed in taskbar
        if let Some(taskbar_hwnd) = native_interop::find_taskbar() {
            diagnose::log(format!("taskbar found hwnd={:?}", taskbar_hwnd));
            native_interop::embed_in_taskbar(hwnd, taskbar_hwnd);
            embedded = true;

            let mut state = lock_state();
            let s = state.as_mut().unwrap();
            s.taskbar_hwnd = Some(taskbar_hwnd);
            s.embedded = true;

            let tray_notify = native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd");
            s.tray_notify_hwnd = tray_notify;
            if tray_notify.is_some() {
                diagnose::log("TrayNotifyWnd found");
            } else {
                diagnose::log("TrayNotifyWnd not found");
            }

            if let Some(tray_hwnd) = tray_notify {
                let thread_id = native_interop::get_window_thread_id(tray_hwnd);
                let hook = native_interop::set_tray_event_hook(thread_id, on_tray_location_changed);
                s.win_event_hook = hook;
                if hook.is_some() {
                    diagnose::log("tray event hook installed");
                } else {
                    diagnose::log("tray event hook could not be installed");
                }
            }
        } else {
            diagnose::log("taskbar not found; using fallback popup window");
        }

        // If not embedded, fall back to topmost popup with SetLayeredWindowAttributes
        if !embedded {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }

        // Create hover popup window
        let popup = create_hover_popup(hinstance);
        {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.popup_hwnd = popup;
            }
        }

        // Create the hidden click panel.
        let panel_hwnd = create_panel_window(hinstance);
        {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.panel.hwnd = panel_hwnd;
            }
        }

        // Register system tray icon(s)
        sync_tray_icons(hwnd);

        // Position and show (only if widget_visible preference is true)
        position_at_taskbar();
        if settings.widget_visible {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        diagnose::log("window shown");

        // If the panel was pinned when the app last ran, restore it immediately.
        if settings.panel_pinned {
            show_panel(hwnd);
        }

        // Initial render via UpdateLayeredWindow (for embedded) or InvalidateRect (fallback)
        render_layered();

        // Poll timer: 15 minutes
        let initial_poll_ms = {
            let state = lock_state();
            state
                .as_ref()
                .map(|s| s.poll_interval_ms)
                .unwrap_or(POLL_15_MIN)
        };
        SetTimer(hwnd, TIMER_POLL, initial_poll_ms, None);

        // Initial poll
        let send_hwnd = SendHwnd::from_hwnd(hwnd);
        std::thread::spawn(move || {
            diagnose::log("initial poll thread started");
            do_poll(send_hwnd);
        });

        schedule_auto_update_check(hwnd);
        let should_check_updates = {
            let state = lock_state();
            state
                .as_ref()
                .map(|s| auto_update_check_due(s.last_update_check_unix))
                .unwrap_or(false)
        };
        if should_check_updates {
            begin_update_check(hwnd, false);
        }

        // Initial theme check
        check_theme_change();

        // Message loop
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Render widget content and push to the layered window via UpdateLayeredWindow.
/// Renders fully opaque with the actual taskbar background colour so that
/// ClearType sub-pixel font rendering can be used for crisp, OS-native text.
fn render_layered() {
    refresh_dpi();
    let (
        hwnd_val,
        is_dark,
        embedded,
        strings,
        session_pct,
        session_text,
        session_resets_at,
        show_claude_code,
    ) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.hwnd,
                s.is_dark,
                s.embedded,
                s.language.strings(),
                s.session_percent,
                s.session_text.clone(),
                s.session_resets_at,
                s.show_claude_code,
            ),
            None => return,
        }
    };

    let hwnd = hwnd_val.to_hwnd();

    // For non-embedded fallback, just invalidate and let WM_PAINT handle it
    if !embedded {
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
        return;
    }

    let width = total_widget_width();
    let height = sc(WIDGET_HEIGHT);

    let panel_colors = PanelColors::new(is_dark);
    let widget_state = state_from_utilization(Some(session_pct));
    let accent = panel_colors.session_accent(widget_state);
    let track = panel_colors.session_soft(widget_state);
    let text_color = if is_dark {
        Color::from_hex("#888888")
    } else {
        Color::from_hex("#404040")
    };
    let bg_color = if is_dark {
        Color::from_hex("#1C1C1C")
    } else {
        Color::from_hex("#F3F3F3")
    };

    unsafe {
        let screen_dc = GetDC(hwnd);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let mem_dc = CreateCompatibleDC(screen_dc);
        let dib =
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();

        if dib.is_invalid() || bits.is_null() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(hwnd, screen_dc);
            return;
        }

        let old_bmp = SelectObject(mem_dc, dib);
        let pixel_count = (width * height) as usize;

        // Render once with the actual taskbar background colour.
        // Using an opaque background lets us use CLEARTYPE_QUALITY for
        // sub-pixel font rendering that matches the rest of the OS.
        paint_content(
            mem_dc,
            width,
            height,
            is_dark,
            &bg_color,
            &text_color,
            &accent,
            &track,
            strings,
            session_pct,
            &session_text,
            session_resets_at,
            show_claude_code,
        );

        // Background pixels → alpha 1 (nearly invisible but still hittable for right-click).
        // Content pixels → fully opaque (preserves ClearType sub-pixel rendering).
        let bg_bgr = bg_color.to_colorref();
        let pixel_data = std::slice::from_raw_parts_mut(bits as *mut u32, pixel_count);
        for px in pixel_data.iter_mut() {
            let rgb = *px & 0x00FFFFFF;
            if rgb == bg_bgr {
                *px = 0x01000000;
            } else {
                *px = rgb | 0xFF000000;
            }
        }

        // Push to window via UpdateLayeredWindow
        let pt_src = POINT { x: 0, y: 0 };
        let sz = SIZE {
            cx: width,
            cy: height,
        };
        let blend = BLENDFUNCTION {
            BlendOp: 0, // AC_SRC_OVER
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: 1, // AC_SRC_ALPHA
        };

        let _ = UpdateLayeredWindow(
            hwnd,
            screen_dc,
            None,
            Some(&sz),
            mem_dc,
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );

        // Cleanup
        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(hwnd, screen_dc);
    }
}

/// Paint all widget content onto a DC with a given background color.
fn paint_content(
    hdc: HDC,
    width: i32,
    height: i32,
    is_dark: bool,
    bg: &Color,
    _text_color: &Color,
    accent: &Color,
    track: &Color,
    _strings: Strings,
    session_pct: f64,
    session_text: &str,
    session_resets_at: Option<SystemTime>,
    show_claude_code: bool,
) {
    unsafe {
        let client_rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };

        let bg_brush = CreateSolidBrush(COLORREF(bg.to_colorref()));
        FillRect(hdc, &client_rect, bg_brush);
        let _ = DeleteObject(bg_brush);

        // Left divider — rounded pill
        let divider_h = sc(25);
        let divider_top = (height - divider_h) / 2;
        let divider_w = sc(3);
        let div_color = if is_dark {
            Color::new(80, 80, 80)
        } else {
            Color::new(160, 160, 160)
        };
        gdip_fill_rounded(hdc, 0, divider_top, divider_w, divider_top + divider_h, divider_w / 2, div_color);

        let content_x = sc(LEFT_DIVIDER_W) + sc(DIVIDER_RIGHT_MARGIN);
        let bar_w = width - content_x - sc(BAR_RIGHT_MARGIN) - sc(RIGHT_MARGIN);
        let bar_h = sc(8);
        let text_h = sc(14);
        let gap = sc(5);
        let content_h = text_h + gap + bar_h;
        let top_y = (height - content_h) / 2;
        let text_y = top_y;
        let bar_y = top_y + text_h + gap;

        let reset_str = native_interop::format_local_reset_time(session_resets_at, false);

        let _ = SetBkMode(hdc, TRANSPARENT);

        draw_session_row(
            hdc,
            content_x,
            text_y,
            bar_y,
            text_h,
            bar_h,
            bar_w,
            session_pct,
            session_text,
            &reset_str,
            show_claude_code,
            accent,
            track,
        );
    }
}

fn poll_error_status(error: &poller::PollError) -> UsagePollStatus {
    match error {
        poller::PollError::AuthRequired => UsagePollStatus::AuthRequired,
        poller::PollError::NoCredentials => UsagePollStatus::NoCredentials,
        poller::PollError::TokenExpired => UsagePollStatus::TokenExpired,
        poller::PollError::RequestFailed => UsagePollStatus::RequestFailed,
    }
}

fn do_poll(send_hwnd: SendHwnd) {
    let hwnd = send_hwnd.to_hwnd();
    let poll_completed_at = SystemTime::now();
    let show_claude_code = {
        let state = lock_state();
        state.as_ref().map(|s| s.show_claude_code).unwrap_or(true)
    };

    match poller::poll(show_claude_code) {
        Ok(data) => {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                if let Some(claude_code) = data.claude_code.as_ref() {
                    s.session_percent = claude_code.session.percentage;
                    s.weekly_percent = claude_code.weekly.percentage;
                } else if s.show_claude_code {
                    s.session_percent = 0.0;
                    s.weekly_percent = 0.0;
                }
                // Stop fast-poll if reset data is now fresh
                if !poller::app_is_past_reset(&data) {
                    unsafe {
                        let _ = KillTimer(hwnd, TIMER_RESET_POLL);
                    }
                }

                s.data = Some(data);
                s.last_poll_ok = true;
                s.last_poll_at = Some(poll_completed_at);
                s.last_poll_status = Some(UsagePollStatus::Success);
                refresh_usage_texts(s);

                // Recovered from errors — restore normal poll interval
                if s.retry_count > 0 {
                    s.retry_count = 0;
                    let interval = s.poll_interval_ms;
                    unsafe {
                        SetTimer(hwnd, TIMER_POLL, interval, None);
                    }
                }
                s.force_notify_auth_error = false;
                s.auth_error_paused_polling = false;
                s.auth_watch_mode = poller::CredentialWatchMode::ActiveSource;
                s.auth_watch_snapshot.clear();
            }

            unsafe {
                let _ = PostMessageW(hwnd, WM_APP_USAGE_UPDATED, WPARAM(0), LPARAM(0));
            }
        }
        Err(e) => {
            let poll_status = poll_error_status(&e);
            let auth_watch = match e {
                poller::PollError::AuthRequired | poller::PollError::TokenExpired => Some((
                    poller::CredentialWatchMode::ActiveSource,
                    poller::credential_watch_snapshot(poller::CredentialWatchMode::ActiveSource),
                )),
                poller::PollError::NoCredentials => Some((
                    poller::CredentialWatchMode::AllSources,
                    poller::credential_watch_snapshot(poller::CredentialWatchMode::AllSources),
                )),
                poller::PollError::RequestFailed => None,
            };
            // Distinguish auth-required errors from transient errors.
            let notify_auth_error = {
                let mut state = lock_state();
                let mut should_notify = false;
                if let Some(s) = state.as_mut() {
                    s.last_poll_ok = false;
                    s.last_poll_at = Some(poll_completed_at);
                    s.last_poll_status = Some(poll_status);
                    match auth_watch {
                        Some((watch_mode, watch_snapshot)) => {
                            // Only show the balloon on the first failure so it doesn't spam.
                            if s.retry_count == 0 || s.force_notify_auth_error {
                                should_notify = true;
                            }
                            s.force_notify_auth_error = false;
                            s.auth_error_paused_polling = true;
                            s.auth_watch_mode = watch_mode;
                            s.auth_watch_snapshot = watch_snapshot;
                            s.session_text = "!".to_string();
                            s.weekly_text = "!".to_string();
                            s.retry_count = s.retry_count.saturating_add(1);
                            unsafe {
                                let _ = KillTimer(hwnd, TIMER_POLL);
                                let _ = KillTimer(hwnd, TIMER_RESET_POLL);
                                let _ = KillTimer(hwnd, TIMER_COUNTDOWN);
                                SetTimer(hwnd, TIMER_POLL, s.poll_interval_ms, None);
                            }
                        }
                        _ => {
                            // Transient network / credential-missing errors: exponential backoff.
                            s.force_notify_auth_error = false;
                            s.auth_error_paused_polling = false;
                            s.auth_watch_mode = poller::CredentialWatchMode::ActiveSource;
                            s.auth_watch_snapshot.clear();
                            s.session_text = "...".to_string();
                            s.weekly_text = "...".to_string();
                            s.retry_count = s.retry_count.saturating_add(1);
                            let backoff = RETRY_BASE_MS.saturating_mul(
                                1u32.checked_shl(s.retry_count - 1).unwrap_or(u32::MAX),
                            );
                            let retry_ms = backoff.min(s.poll_interval_ms);
                            unsafe {
                                let _ = KillTimer(hwnd, TIMER_RESET_POLL);
                                SetTimer(hwnd, TIMER_POLL, retry_ms, None);
                            }
                        }
                    }
                }
                should_notify
            };

            if notify_auth_error {
                let balloon = {
                    let state = lock_state();
                    state.as_ref().map(|s| {
                        (
                            tray_icon::TrayIconKind::Claude,
                            s.language.strings().token_expired_title,
                            s.language.strings().token_expired_body,
                        )
                    })
                };
                if let Some((kind, title, body)) = balloon {
                    tray_icon::notify_balloon(hwnd, kind, title, body);
                }
            }

            unsafe {
                let _ = PostMessageW(hwnd, WM_APP_USAGE_UPDATED, WPARAM(0), LPARAM(0));
            }
        }
    }
}

fn schedule_countdown_timer() {
    let state = lock_state();
    let s = match state.as_ref() {
        Some(s) => s,
        None => return,
    };

    let hwnd = s.hwnd.to_hwnd();
    if !s.last_poll_ok {
        unsafe {
            let _ = KillTimer(hwnd, TIMER_COUNTDOWN);
            let _ = KillTimer(hwnd, TIMER_RESET_POLL);
        }
        return;
    }

    let data = match &s.data {
        Some(d) => d,
        None => return,
    };

    // If a reset time has passed, poll every 5s to pick up fresh data
    if poller::app_is_past_reset(data) {
        unsafe {
            SetTimer(hwnd, TIMER_RESET_POLL, 5_000, None);
        }
    }

    let delays = [
        data.claude_code
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.session.resets_at)),
        data.claude_code
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.weekly.resets_at)),
    ];
    let min_delay = delays.into_iter().flatten().min();

    let ms = min_delay
        .unwrap_or(Duration::from_secs(60))
        .as_millis()
        .max(1000) as u32;

    unsafe {
        SetTimer(hwnd, TIMER_COUNTDOWN, ms, None);
    }
}

fn check_theme_change() {
    let new_dark = theme::is_dark_mode();
    let changed = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            if s.is_dark != new_dark {
                s.is_dark = new_dark;
                true
            } else {
                false
            }
        } else {
            false
        }
    };
    if changed {
        render_layered();
    }
}

fn check_language_change() {
    if update_language_change() {
        render_layered();
    }
}

fn update_display() {
    let mut state = lock_state();
    let s = match state.as_mut() {
        Some(s) => s,
        None => return,
    };

    // Don't overwrite error text with stale cached data
    if !s.last_poll_ok {
        return;
    }

    refresh_usage_texts(s);
}

fn suppress_tray_reposition_for(duration: Duration) {
    let mut until = SUPPRESS_TRAY_REPOSITION_UNTIL
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *until = Some(Instant::now() + duration);
}

fn tray_reposition_is_suppressed() -> bool {
    let now = Instant::now();
    let mut until = SUPPRESS_TRAY_REPOSITION_UNTIL
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    match *until {
        Some(deadline) if now < deadline => true,
        Some(_) => {
            *until = None;
            false
        }
        None => false,
    }
}

fn position_at_taskbar() {
    refresh_dpi();
    // Drop the app-state lock before any Win32 call that may synchronously
    // re-enter our window procedure.
    let (hwnd, embedded, tray_offset, taskbar_hwnd) = {
        let state = lock_state();
        let s = match state.as_ref() {
            Some(s) => s,
            None => return,
        };

        // Don't fight the user's drag
        if s.dragging {
            return;
        }

        let taskbar_hwnd = match s.taskbar_hwnd {
            Some(h) => h,
            None => {
                diagnose::log("position_at_taskbar skipped: no taskbar handle");
                return;
            }
        };

        (s.hwnd.to_hwnd(), s.embedded, s.tray_offset, taskbar_hwnd)
    };

    let taskbar_rect = match native_interop::get_taskbar_rect(taskbar_hwnd) {
        Some(r) => r,
        None => {
            diagnose::log("position_at_taskbar skipped: unable to query taskbar rect");
            return;
        }
    };

    let taskbar_height = taskbar_rect.bottom - taskbar_rect.top;
    let mut tray_left = taskbar_rect.right;
    let anchor_top = taskbar_rect.top;
    let anchor_height = taskbar_height;

    if let Some(tray_hwnd) = native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd") {
        if let Some(tray_rect) = native_interop::get_window_rect_safe(tray_hwnd) {
            tray_left = tray_rect.left;
        }
    }

    let widget_width = total_widget_width();

    let widget_height = sc(WIDGET_HEIGHT);
    let y = compute_anchor_y(anchor_top, anchor_height, widget_height);
    if embedded {
        // Child window: coordinates relative to parent (taskbar)
        let x = tray_left - taskbar_rect.left - widget_width - tray_offset;
        native_interop::move_window(hwnd, x, y - taskbar_rect.top, widget_width, widget_height);
        diagnose::log(format!(
            "positioned embedded widget at x={x} y={} w={widget_width} h={widget_height}",
            y - taskbar_rect.top
        ));
    } else {
        // Topmost popup: screen coordinates
        let x = tray_left - widget_width - tray_offset;
        native_interop::move_window(hwnd, x, y, widget_width, widget_height);
        diagnose::log(format!(
            "positioned fallback widget at x={x} y={y} w={widget_width} h={widget_height}"
        ));
    }
}

fn compute_anchor_y(anchor_top: i32, anchor_height: i32, widget_height: i32) -> i32 {
    let anchor_bottom = anchor_top + anchor_height;
    (anchor_bottom - widget_height).max(anchor_top)
}

/// WinEvent callback for tray icon location changes
unsafe extern "system" fn on_tray_location_changed(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    static LAST_REPOSITION: Mutex<Option<std::time::Instant>> = Mutex::new(None);

    let is_tray = {
        let state = lock_state();
        state
            .as_ref()
            .and_then(|s| s.tray_notify_hwnd)
            .map(|h| h == hwnd)
            .unwrap_or(false)
    };

    if is_tray {
        if tray_reposition_is_suppressed() {
            return;
        }

        let should_reposition = {
            let mut last = LAST_REPOSITION.lock().unwrap_or_else(|e| e.into_inner());
            let now = std::time::Instant::now();
            if last
                .map(|t| now.duration_since(t).as_millis() > 500)
                .unwrap_or(true)
            {
                *last = Some(now);
                true
            } else {
                false
            }
        };
        if should_reposition {
            position_at_taskbar();
            render_layered();
        }
    }
}

/// Main window procedure
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            // For non-embedded fallback, paint normally
            let embedded = {
                let state = lock_state();
                state.as_ref().map(|s| s.embedded).unwrap_or(false)
            };
            if embedded {
                // Layered windows don't use WM_PAINT; just validate the region
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
            } else {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                paint(hdc, hwnd);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_DISPLAYCHANGE | WM_DPICHANGED_MSG | WM_SETTINGCHANGE => {
            if msg == WM_DPICHANGED_MSG {
                let new_dpi = (wparam.0 & 0xFFFF) as u32;
                CURRENT_DPI.store(new_dpi, Ordering::Relaxed);
            }
            if msg == WM_SETTINGCHANGE {
                check_theme_change();
                check_language_change();
            }
            refresh_dpi();
            position_at_taskbar();
            refresh_panel_view(hwnd);
            render_layered();
            LRESULT(0)
        }
        WM_TIMER => {
            let timer_id = wparam.0;
            match timer_id {
                TIMER_POLL => {
                    let auth_watch = {
                        let state = lock_state();
                        state.as_ref().map(|s| {
                            (
                                s.auth_error_paused_polling,
                                s.auth_watch_mode,
                                s.auth_watch_snapshot.clone(),
                            )
                        })
                    };
                    match auth_watch {
                        Some((true, watch_mode, previous_snapshot)) => {
                            let current_snapshot = poller::credential_watch_snapshot(watch_mode);
                            if current_snapshot != previous_snapshot {
                                let mut state = lock_state();
                                if let Some(s) = state.as_mut() {
                                    if s.auth_error_paused_polling
                                        && s.auth_watch_mode == watch_mode
                                    {
                                        s.auth_watch_snapshot = current_snapshot;
                                    }
                                }
                                drop(state);
                                let sh = SendHwnd::from_hwnd(hwnd);
                                std::thread::spawn(move || {
                                    do_poll(sh);
                                });
                            }
                        }
                        Some((false, _, _)) => {
                            let sh = SendHwnd::from_hwnd(hwnd);
                            std::thread::spawn(move || {
                                do_poll(sh);
                            });
                        }
                        None => {}
                    }
                }
                TIMER_COUNTDOWN => {
                    update_display();
                    render_layered();
                    refresh_panel_view(hwnd);
                    schedule_countdown_timer();
                }
                TIMER_RESET_POLL => {
                    let should_poll = {
                        let state = lock_state();
                        state
                            .as_ref()
                            .map(|s| !s.auth_error_paused_polling)
                            .unwrap_or(false)
                    };
                    if should_poll {
                        let sh = SendHwnd::from_hwnd(hwnd);
                        std::thread::spawn(move || {
                            do_poll(sh);
                        });
                    }
                }
                TIMER_UPDATE_CHECK => {
                    begin_update_check(hwnd, false);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_USAGE_UPDATED => {
            check_theme_change();
            check_language_change();
            render_layered();
            refresh_panel_view(hwnd);
            schedule_countdown_timer();
            suppress_tray_reposition_for(Duration::from_millis(
                TRAY_ICON_UPDATE_REPOSITION_SUPPRESS_MS,
            ));
            sync_tray_icons(hwnd);
            LRESULT(0)
        }
        WM_APP_UPDATE_CHECK_COMPLETE => {
            schedule_auto_update_check(hwnd);
            LRESULT(0)
        }
        WM_MOUSEACTIVATE => {
            // When the panel is open and the user clicks the widget (e.g. the drag handle),
            // returning MA_NOACTIVATE lets the click pass straight to WM_LBUTTONDOWN without
            // triggering the WM_ACTIVATE → hide_panel() roundtrip that would eat the gesture.
            let (panel_visible, pinned) = lock_state()
                .as_ref()
                .map(|s| (s.panel.visible, s.panel_pinned))
                .unwrap_or((false, false));
            if panel_visible && !pinned {
                hide_panel();
                return LRESULT(3); // MA_NOACTIVATE
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_SETCURSOR => {
            let is_dragging = {
                let state = lock_state();
                state.as_ref().map(|s| s.dragging).unwrap_or(false)
            };
            // Always show resize cursor while dragging or when hovering divider zone
            let hit_test = (lparam.0 & 0xFFFF) as u16;
            if is_dragging {
                let cursor = LoadCursorW(HINSTANCE::default(), IDC_SIZEWE).unwrap_or_default();
                SetCursor(cursor);
                return LRESULT(1);
            }
            if hit_test == 1 {
                // HTCLIENT
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = ScreenToClient(hwnd, &mut pt);
                if pt.x < sc(DIVIDER_HIT_ZONE) {
                    let cursor = LoadCursorW(HINSTANCE::default(), IDC_SIZEWE).unwrap_or_default();
                    SetCursor(cursor);
                    return LRESULT(1);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            let client_x = (lparam.0 & 0xFFFF) as i16 as i32;
            if client_x < sc(DIVIDER_HIT_ZONE) {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    s.dragging = true;
                    s.drag_start_mouse_x = pt.x;
                    s.drag_start_offset = s.tray_offset;
                }
                SetCapture(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let is_dragging = {
                let state = lock_state();
                state.as_ref().map(|s| s.dragging).unwrap_or(false)
            };
            if is_dragging {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let move_target = {
                    let mut state = lock_state();
                    let s = match state.as_mut() {
                        Some(s) => s,
                        None => return LRESULT(0),
                    };

                    // Moving mouse left = positive delta = larger offset (further left)
                    let delta = s.drag_start_mouse_x - pt.x;
                    let mut new_offset = s.drag_start_offset + delta;

                    // Clamp: offset >= 0 (can't go right of default)
                    if new_offset < 0 {
                        new_offset = 0;
                    }

                    let taskbar_hwnd = s.taskbar_hwnd;
                    let embedded = s.embedded;
                    let hwnd_val = s.hwnd.to_hwnd();

                    // Clamp: don't go past left edge of taskbar
                    if let Some(taskbar_hwnd) = taskbar_hwnd {
                        if let Some(taskbar_rect) = native_interop::get_taskbar_rect(taskbar_hwnd) {
                            let mut tray_left = taskbar_rect.right;
                            if let Some(tray_hwnd) =
                                native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd")
                            {
                                if let Some(tray_rect) =
                                    native_interop::get_window_rect_safe(tray_hwnd)
                                {
                                    tray_left = tray_rect.left;
                                }
                            }
                            let widget_width = total_widget_width_for_state(s);
                            let max_offset = (tray_left - taskbar_rect.left - widget_width).max(0);
                            if new_offset > max_offset {
                                new_offset = max_offset;
                            }

                            s.tray_offset = new_offset;

                            let taskbar_height = taskbar_rect.bottom - taskbar_rect.top;
                            let anchor_top = taskbar_rect.top;
                            let anchor_height = taskbar_height;
                            let widget_height = sc(WIDGET_HEIGHT);
                            let y = compute_anchor_y(anchor_top, anchor_height, widget_height);
                            let x = if embedded {
                                tray_left - taskbar_rect.left - widget_width - new_offset
                            } else {
                                tray_left - widget_width - new_offset
                            };
                            Some((
                                hwnd_val,
                                embedded,
                                x,
                                y,
                                taskbar_rect.top,
                                widget_width,
                                widget_height,
                            ))
                        } else {
                            s.tray_offset = new_offset;
                            None
                        }
                    } else {
                        s.tray_offset = new_offset;
                        None
                    }
                };

                if let Some((hwnd_val, embedded, x, y, taskbar_top, widget_width, widget_height)) =
                    move_target
                {
                    if embedded {
                        native_interop::move_window(
                            hwnd_val,
                            x,
                            y - taskbar_top,
                            widget_width,
                            widget_height,
                        );
                    } else {
                        native_interop::move_window(hwnd_val, x, y, widget_width, widget_height);
                    }
                    position_panel_near_widget(hwnd);
                }
            }
            let should_show = lock_state().as_mut().map_or(false, |s| {
                if !s.dragging && !s.mouse_over_widget {
                    s.mouse_over_widget = true;
                    true
                } else {
                    false
                }
            });
            if should_show {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
                show_hover_popup(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    s.mouse_over_widget = false;
                }
            }
            hide_hover_popup();
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let client_x = (lparam.0 & 0xFFFF) as i16 as i32;
            let was_dragging = {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    if s.dragging {
                        s.dragging = false;
                        let offset = s.tray_offset;
                        Some(offset)
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if was_dragging.is_some() {
                let _ = ReleaseCapture();
                save_state_settings();
            } else if client_x >= sc(DIVIDER_HIT_ZONE) {
                toggle_panel(hwnd);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            show_context_menu(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = wparam.0 as u16;
            match id {
                1 => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.session_text = "...".to_string();
                            s.weekly_text = "...".to_string();
                            s.force_notify_auth_error = true;
                        }
                    }
                    render_layered();
                    let sh = SendHwnd::from_hwnd(hwnd);
                    std::thread::spawn(move || {
                        do_poll(sh);
                    });
                }
                IDM_VERSION_ACTION => {
                    let (install_channel, release) = {
                        let state = lock_state();
                        match state.as_ref() {
                            Some(s) => (
                                s.install_channel,
                                match &s.update_status {
                                    UpdateStatus::Available(release) => Some(release.clone()),
                                    _ => None,
                                },
                            ),
                            None => (InstallChannel::Portable, None),
                        }
                    };

                    match install_channel {
                        InstallChannel::Winget => {
                            if release.is_some() {
                                begin_winget_update(hwnd);
                            } else {
                                begin_update_check(hwnd, true);
                            }
                        }
                        InstallChannel::Portable => {
                            if let Some(release) = release {
                                begin_update_apply(hwnd, release);
                            } else {
                                begin_update_check(hwnd, true);
                            }
                        }
                    }
                }
                2 => {
                    let hook = {
                        let state = lock_state();
                        state.as_ref().and_then(|s| s.win_event_hook)
                    };
                    if let Some(h) = hook {
                        native_interop::unhook_win_event(h);
                    }
                    PostQuitMessage(0);
                }
                IDM_RESET_POSITION => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.tray_offset = 0;
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                }
                IDM_START_WITH_WINDOWS => {
                    set_startup_enabled(!is_startup_enabled());
                }
                IDM_FREQ_1MIN | IDM_FREQ_5MIN | IDM_FREQ_15MIN | IDM_FREQ_1HOUR => {
                    let new_interval = match id {
                        IDM_FREQ_1MIN => POLL_1_MIN,
                        IDM_FREQ_5MIN => POLL_5_MIN,
                        IDM_FREQ_15MIN => POLL_15_MIN,
                        IDM_FREQ_1HOUR => POLL_1_HOUR,
                        _ => POLL_15_MIN,
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.poll_interval_ms = new_interval;
                        }
                    }
                    save_state_settings();
                    // Reset the poll timer with the new interval
                    SetTimer(hwnd, TIMER_POLL, new_interval, None);
                }
                IDM_FORMAT_LONG | IDM_FORMAT_SHORT => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.compound_countdown = id == IDM_FORMAT_LONG;
                            refresh_usage_texts(s);
                        }
                    }
                    save_state_settings();
                    render_layered();
                    refresh_panel_view(hwnd);
                    schedule_countdown_timer();
                }
                IDM_PANEL_BG_SOLID | IDM_PANEL_BG_TRANSLUCENT => {
                    let new_bg = match id {
                        IDM_PANEL_BG_SOLID => PanelBackground::Solid,
                        _ => PanelBackground::Translucent,
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.panel_background = new_bg;
                        }
                    }
                    save_state_settings();
                    // Applied on next show_panel — don't touch the hidden window here.
                }
                IDM_MODEL_CLAUDE_CODE => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.show_claude_code = !s.show_claude_code;
                            if !s.show_claude_code {
                                s.show_claude_code = true; // always keep Claude on
                            }
                            s.session_text = "...".to_string();
                            s.weekly_text = "...".to_string();
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                    refresh_panel_view(hwnd);
                    render_layered();
                    sync_tray_icons(hwnd);
                    let sh = SendHwnd::from_hwnd(hwnd);
                    std::thread::spawn(move || {
                        do_poll(sh);
                    });
                }
                IDM_LANG_SYSTEM
                | IDM_LANG_ENGLISH
                | IDM_LANG_DUTCH
                | IDM_LANG_SPANISH
                | IDM_LANG_FRENCH
                | IDM_LANG_GERMAN
                | IDM_LANG_JAPANESE
                | IDM_LANG_KOREAN
                | IDM_LANG_TRADITIONAL_CHINESE => {
                    let language_override = match id {
                        IDM_LANG_SYSTEM => None,
                        IDM_LANG_ENGLISH => Some(LanguageId::English),
                        IDM_LANG_DUTCH => Some(LanguageId::Dutch),
                        IDM_LANG_SPANISH => Some(LanguageId::Spanish),
                        IDM_LANG_FRENCH => Some(LanguageId::French),
                        IDM_LANG_GERMAN => Some(LanguageId::German),
                        IDM_LANG_JAPANESE => Some(LanguageId::Japanese),
                        IDM_LANG_KOREAN => Some(LanguageId::Korean),
                        IDM_LANG_TRADITIONAL_CHINESE => Some(LanguageId::TraditionalChinese),
                        _ => None,
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            apply_language_to_state(s, language_override);
                        }
                    }
                    save_state_settings();
                    render_layered();
                }
                id if id == tray_icon::IDM_TOGGLE_WIDGET => {
                    toggle_widget_visibility(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        _ if msg == WM_APP_TRAY => {
            match tray_icon::handle_message(lparam) {
                tray_icon::TrayAction::ToggleWidget => {
                    toggle_panel(hwnd);
                }
                tray_icon::TrayAction::ShowContextMenu => {
                    show_context_menu(hwnd);
                }
                tray_icon::TrayAction::None => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let (hook, panel_hwnd) = {
                let state = lock_state();
                (
                    state.as_ref().and_then(|s| s.win_event_hook),
                    state.as_ref().and_then(|s| s.panel.hwnd),
                )
            };
            if let Some(h) = hook {
                native_interop::unhook_win_event(h);
            }
            if let Some(panel_hwnd) = panel_hwnd {
                let _ = DestroyWindow(panel_hwnd);
            }
            tray_icon::remove_all(hwnd);
            shutdown_gdiplus();
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn show_context_menu(hwnd: HWND) {
    unsafe {
        let (
            current_interval,
            compound_countdown,
            strings,
            language,
            language_override,
            install_channel,
            update_status,
            widget_visible,
            show_claude_code,
            panel_bg,
        ) = {
            let state = lock_state();
            match state.as_ref() {
                Some(s) => (
                    s.poll_interval_ms,
                    s.compound_countdown,
                    s.language.strings(),
                    s.language,
                    s.language_override,
                    s.install_channel,
                    s.update_status.clone(),
                    s.widget_visible,
                    s.show_claude_code,
                    s.panel_background,
                ),
                None => (
                    POLL_15_MIN,
                    true,
                    LanguageId::English.strings(),
                    LanguageId::English,
                    None,
                    InstallChannel::Portable,
                    UpdateStatus::Idle,
                    true,
                    true,
                    PanelBackground::Solid,
                ),
            }
        };

        let menu = CreatePopupMenu().unwrap();

        let refresh_str = native_interop::wide_str(strings.refresh);
        let _ = AppendMenuW(
            menu,
            MENU_ITEM_FLAGS(0),
            1,
            PCWSTR::from_raw(refresh_str.as_ptr()),
        );

        // Format submenu
        let format_menu = CreatePopupMenu().unwrap();
        let format_items: [(u16, bool, &str); 2] = [
            (IDM_FORMAT_LONG, true, strings.format_long),
            (IDM_FORMAT_SHORT, false, strings.format_short),
        ];
        for (id, is_compound, label) in format_items {
            let flags = if compound_countdown == is_compound {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let label_str = native_interop::wide_str(label);
            let _ = AppendMenuW(
                format_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }
        let format_label = native_interop::wide_str(strings.format);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            format_menu.0 as usize,
            PCWSTR::from_raw(format_label.as_ptr()),
        );

        // Panel Background submenu
        let bg_menu = CreatePopupMenu().unwrap();
        let bg_items: [(u16, PanelBackground, &str); 2] = [
            (IDM_PANEL_BG_SOLID, PanelBackground::Solid, "Solid"),
            (IDM_PANEL_BG_TRANSLUCENT, PanelBackground::Translucent, "Translucent"),
        ];
        for (id, variant, label) in bg_items {
            let flags = if panel_bg == variant { MF_CHECKED } else { MENU_ITEM_FLAGS(0) };
            let label_str = native_interop::wide_str(label);
            let _ = AppendMenuW(bg_menu, flags, id as usize, PCWSTR::from_raw(label_str.as_ptr()));
        }
        let bg_label = native_interop::wide_str("Panel Background");
        let _ = AppendMenuW(menu, MF_POPUP, bg_menu.0 as usize, PCWSTR::from_raw(bg_label.as_ptr()));

        // Update Frequency submenu
        let freq_menu = CreatePopupMenu().unwrap();
        let freq_items: [(u16, u32, &str); 4] = [
            (IDM_FREQ_1MIN, POLL_1_MIN, strings.one_minute),
            (IDM_FREQ_5MIN, POLL_5_MIN, strings.five_minutes),
            (IDM_FREQ_15MIN, POLL_15_MIN, strings.fifteen_minutes),
            (IDM_FREQ_1HOUR, POLL_1_HOUR, strings.one_hour),
        ];
        for (id, interval, label) in freq_items {
            let label_str = native_interop::wide_str(label);
            let flags = if interval == current_interval {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                freq_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }

        let freq_label = native_interop::wide_str(strings.update_frequency);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            freq_menu.0 as usize,
            PCWSTR::from_raw(freq_label.as_ptr()),
        );

        // Models submenu
        let models_menu = CreatePopupMenu().unwrap();
        let claude_model = native_interop::wide_str(strings.claude_code_model);
        let claude_flags = if show_claude_code {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            models_menu,
            claude_flags,
            IDM_MODEL_CLAUDE_CODE as usize,
            PCWSTR::from_raw(claude_model.as_ptr()),
        );

        let models_label = native_interop::wide_str(strings.models);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            models_menu.0 as usize,
            PCWSTR::from_raw(models_label.as_ptr()),
        );

        // Settings submenu
        let settings_menu = CreatePopupMenu().unwrap();

        let startup_str = native_interop::wide_str(strings.start_with_windows);
        let startup_flags = if is_startup_enabled() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            settings_menu,
            startup_flags,
            IDM_START_WITH_WINDOWS as usize,
            PCWSTR::from_raw(startup_str.as_ptr()),
        );

        let reset_pos_str = native_interop::wide_str(strings.reset_position);
        let _ = AppendMenuW(
            settings_menu,
            MENU_ITEM_FLAGS(0),
            IDM_RESET_POSITION as usize,
            PCWSTR::from_raw(reset_pos_str.as_ptr()),
        );

        let language_menu = CreatePopupMenu().unwrap();
        let system_label = native_interop::wide_str(strings.system_default);
        let system_flags = if language_override.is_none() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            language_menu,
            system_flags,
            IDM_LANG_SYSTEM as usize,
            PCWSTR::from_raw(system_label.as_ptr()),
        );

        for language in LanguageId::ALL {
            let id = match language {
                LanguageId::English => IDM_LANG_ENGLISH,
                LanguageId::Dutch => IDM_LANG_DUTCH,
                LanguageId::Spanish => IDM_LANG_SPANISH,
                LanguageId::French => IDM_LANG_FRENCH,
                LanguageId::German => IDM_LANG_GERMAN,
                LanguageId::Japanese => IDM_LANG_JAPANESE,
                LanguageId::Korean => IDM_LANG_KOREAN,
                LanguageId::TraditionalChinese => IDM_LANG_TRADITIONAL_CHINESE,
            };
            let label_str = native_interop::wide_str(language.native_name());
            let flags = if language_override == Some(language) {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                language_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }

        let language_label = native_interop::wide_str(strings.language);
        let _ = AppendMenuW(
            settings_menu,
            MF_POPUP,
            language_menu.0 as usize,
            PCWSTR::from_raw(language_label.as_ptr()),
        );

        let _ = AppendMenuW(settings_menu, MF_SEPARATOR, 0, PCWSTR::null());

        let version_label =
            version_action_label(strings, language, install_channel, &update_status);
        let version_str = native_interop::wide_str(&version_label);
        let version_flags = if matches!(
            update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        ) {
            MF_GRAYED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            settings_menu,
            version_flags,
            IDM_VERSION_ACTION as usize,
            PCWSTR::from_raw(version_str.as_ptr()),
        );

        let settings_label = native_interop::wide_str(strings.settings);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            settings_menu.0 as usize,
            PCWSTR::from_raw(settings_label.as_ptr()),
        );

        let widget_label = native_interop::wide_str(strings.show_widget);
        let widget_flags = if widget_visible {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            menu,
            widget_flags,
            tray_icon::IDM_TOGGLE_WIDGET as usize,
            PCWSTR::from_raw(widget_label.as_ptr()),
        );

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        let exit_str = native_interop::wide_str(strings.exit);
        let _ = AppendMenuW(
            menu,
            MENU_ITEM_FLAGS(0),
            2,
            PCWSTR::from_raw(exit_str.as_ptr()),
        );

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
        let _ = DestroyMenu(menu);
    }
}

/// Paint for non-embedded fallback (normal WM_PAINT path)
fn paint(hdc: HDC, hwnd: HWND) {
    let (
        is_dark,
        strings,
        session_pct,
        session_text,
        session_resets_at,
        show_claude_code,
    ) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.is_dark,
                s.language.strings(),
                s.session_percent,
                s.session_text.clone(),
                s.session_resets_at,
                s.show_claude_code,
            ),
            None => return,
        }
    };

    let panel_colors = PanelColors::new(is_dark);
    let widget_state = state_from_utilization(Some(session_pct));
    let accent = panel_colors.session_accent(widget_state);
    let track = panel_colors.session_soft(widget_state);
    let text_color = if is_dark {
        Color::from_hex("#888888")
    } else {
        Color::from_hex("#404040")
    };
    let bg_color = if is_dark {
        Color::from_hex("#1C1C1C")
    } else {
        Color::from_hex("#F3F3F3")
    };

    unsafe {
        let mut client_rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut client_rect);
        let width = client_rect.right - client_rect.left;
        let height = client_rect.bottom - client_rect.top;

        if width <= 0 || height <= 0 {
            return;
        }

        let mem_dc = CreateCompatibleDC(hdc);
        let mem_bmp = CreateCompatibleBitmap(hdc, width, height);
        let old_bmp = SelectObject(mem_dc, mem_bmp);

        paint_content(
            mem_dc,
            width,
            height,
            is_dark,
            &bg_color,
            &text_color,
            &accent,
            &track,
            strings,
            session_pct,
            &session_text,
            session_resets_at,
            show_claude_code,
        );

        let _ = BitBlt(hdc, 0, 0, width, height, mem_dc, 0, 0, SRCCOPY);

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(mem_bmp);
        let _ = DeleteDC(mem_dc);
    }
}

fn draw_session_row(
    hdc: HDC,
    x: i32,
    text_y: i32,
    bar_y: i32,
    text_h: i32,
    bar_h: i32,
    bar_w: i32,
    claude_percent: f64,
    claude_text: &str,
    reset_str: &str,
    show_claude_code: bool,
    claude_accent: &Color,
    track: &Color,
) {
    let corner_r = bar_h / 2;
    let white = Color::new(255, 255, 255);
    let percent_clamped = claude_percent.clamp(0.0, 100.0);

    // Split "84% · 2h30m" → pct_str="84%", countdown="2h30m"
    let (pct_str, countdown) = match claude_text.split_once(" \u{00b7} ") {
        Some((p, t)) => (p, t),
        None => (claude_text, ""),
    };

    // Right text: "resets at 3:45 PM · 2h30m"
    let right_str = match (reset_str.is_empty(), countdown.is_empty()) {
        (false, false) => format!("resets at {} \u{00b7} {}", reset_str, countdown),
        (false, true)  => format!("resets at {}", reset_str),
        (true,  false) => countdown.to_string(),
        (true,  true)  => String::new(),
    };

    let fill_color = *claude_accent;

    unsafe {
        let font_name = native_interop::wide_str("Segoe UI");

        // % — semibold, white, left-aligned
        let pct_font = CreateFontW(
            sc(-13), 0, 0, 0, 600, 0, 0, 0,
            DEFAULT_CHARSET.0 as u32, OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32, CLEARTYPE_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(font_name.as_ptr()),
        );
        let prev_font = SelectObject(hdc, pct_font);
        let _ = SetTextColor(hdc, COLORREF(white.to_colorref()));
        let mut pct_wide: Vec<u16> = pct_str.encode_utf16().collect();
        let mut pct_rect = RECT { left: x, top: text_y, right: x + bar_w, bottom: text_y + text_h };
        let _ = DrawTextW(hdc, &mut pct_wide, &mut pct_rect, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        SelectObject(hdc, prev_font);
        let _ = DeleteObject(pct_font);

        // Right text — regular, white, right-aligned
        if !right_str.is_empty() {
            let time_font = CreateFontW(
                sc(-12), 0, 0, 0, FW_MEDIUM.0 as i32, 0, 0, 0,
                DEFAULT_CHARSET.0 as u32, OUT_TT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32, CLEARTYPE_QUALITY.0 as u32,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                PCWSTR::from_raw(font_name.as_ptr()),
            );
            let prev_font2 = SelectObject(hdc, time_font);
            let _ = SetTextColor(hdc, COLORREF(white.to_colorref()));
            let mut right_wide: Vec<u16> = right_str.encode_utf16().collect();
            let mut right_rect = RECT { left: x, top: text_y, right: x + bar_w, bottom: text_y + text_h };
            let _ = DrawTextW(hdc, &mut right_wide, &mut right_rect, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
            SelectObject(hdc, prev_font2);
            let _ = DeleteObject(time_font);
        }

        // Continuous pill bar — GDI+ anti-aliased
        if show_claude_code {
            gdip_fill_rounded(hdc, x, bar_y, x + bar_w, bar_y + bar_h, corner_r, *track);

            let fill_w = ((bar_w as f64) * percent_clamped / 100.0).round() as i32;
            if fill_w > corner_r {
                gdip_fill_rounded(hdc, x, bar_y, x + fill_w, bar_y + bar_h, corner_r, fill_color);
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    // --- settings backward compatibility ---

    #[test]
    fn sanitize_clamps_invalid_poll_interval() {
        let mut s = SettingsFile {
            poll_interval_ms: 99999,
            ..SettingsFile::default()
        };
        sanitize_settings(&mut s);
        assert_eq!(s.poll_interval_ms, POLL_15_MIN);
    }

    #[test]
    fn settings_two_pass_recovers_known_fields_from_bad_json() {
        // JSON where poll_interval_ms is an invalid type; other fields should survive.
        let json = r#"{"poll_interval_ms": "bad", "show_claude_code": true}"#;
        let raw: serde_json::Value = serde_json::from_str(json).unwrap();
        let d = SettingsFile::default();
        let show_claude_code = raw["show_claude_code"].as_bool().unwrap_or(d.show_claude_code);
        assert!(
            show_claude_code,
            "slow path should recover show_claude_code from partially bad JSON"
        );
        let poll = raw["poll_interval_ms"]
            .as_u64()
            .map(|v| v as u32)
            .unwrap_or(d.poll_interval_ms);
        assert_eq!(
            poll, POLL_15_MIN,
            "slow path should fall back to default for bad field"
        );
    }
}
