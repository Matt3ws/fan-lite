//! Notification-area ("tray") icon, as lghub-lite: closing the window hides it here and fan
//! control keeps running. Left click shows the window, right click opens Open / Quit.
//!
//! Plain Win32 through `windows-sys`. Messages arrive in our window procedure (see
//! `chrome::wndproc`), which turns them into flags that `App::logic` picks up.

use std::f32::consts::TAU;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use eframe::egui;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIcon, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos, GetSystemMetrics, HICON,
    MF_SEPARATOR, MF_STRING, PostMessageW, RegisterWindowMessageW, SM_CXSMICON, SetForegroundWindow, TPM_NONOTIFY,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WM_APP, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP,
};

use crate::hw::win::wide;

/// Window message the shell sends for clicks on our icon.
const WM_TRAY: u32 = WM_APP + 1;
const ICON_ID: u32 = 1;
const MENU_OPEN: usize = 1;
const MENU_QUIT: usize = 2;

pub const SHOW: u8 = 1;
pub const QUIT: u8 = 2;
/// Explorer restarted: the icon must be added again.
pub const READD: u8 = 4;

static EVENTS: AtomicU8 = AtomicU8::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static CTX: OnceLock<egui::Context> = OnceLock::new();

/// Lets the window procedure wake the app when the icon is clicked while the window is hidden.
pub fn set_ctx(ctx: egui::Context) {
    let _ = CTX.set(ctx);
}

/// Take and clear pending SHOW / QUIT / READD flags.
pub fn take_events() -> u8 {
    EVENTS.swap(0, Ordering::AcqRel)
}

fn post(flag: u8) {
    EVENTS.fetch_or(flag, Ordering::AcqRel);
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

/// Called from the window procedure for every message; `Some` means handled.
pub fn handle_message(hwnd: HWND, msg: u32, _wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    if msg == WM_TRAY {
        match lparam as u32 {
            WM_LBUTTONUP => post(SHOW),
            WM_RBUTTONUP => match unsafe { context_menu(hwnd) } {
                MENU_OPEN => post(SHOW),
                MENU_QUIT => post(QUIT),
                _ => {}
            },
            _ => {}
        }
        return Some(0);
    }
    let created = TASKBAR_CREATED.load(Ordering::Relaxed);
    if created != 0 && msg == created {
        post(READD);
    }
    None
}

/// Shows Open / Quit at the cursor and returns the chosen item (0 = dismissed).
unsafe fn context_menu(hwnd: HWND) -> usize {
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return 0;
        }
        let open = wide("Open fan-lite");
        let quit = wide("Quit (fans return to firmware control)");
        AppendMenuW(menu, MF_STRING, MENU_OPEN, open.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, MENU_QUIT, quit.as_ptr());
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        // Required so the menu closes when the user clicks elsewhere.
        SetForegroundWindow(hwnd);
        let choice = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            0,
            hwnd,
            std::ptr::null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        choice as usize
    }
}

pub struct Tray {
    data: NOTIFYICONDATAW,
    icon: HICON,
    tip: String,
}

impl Tray {
    /// Adds the icon. `None` if the shell refused (e.g. Explorer not running), in which
    /// case closing the window should quit as before.
    pub fn new(hwnd: HWND) -> Option<Self> {
        let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.clamp(16, 64) as u32;
        let icon = make_hicon(size)?;
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = ICON_ID;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = icon;
        let mut tray = Tray {
            data,
            icon,
            tip: String::new(),
        };
        tray.write_tip("fan-lite");
        let w = wide("TaskbarCreated");
        TASKBAR_CREATED.store(unsafe { RegisterWindowMessageW(w.as_ptr()) }, Ordering::Relaxed);
        if !tray.add() {
            unsafe { DestroyIcon(icon) };
            return None;
        }
        Some(tray)
    }

    /// (Re-)add the icon to the notification area.
    pub fn add(&mut self) -> bool {
        unsafe { Shell_NotifyIconW(NIM_ADD, &self.data) != 0 }
    }

    /// Update the hover text (one line per item); skipped when unchanged.
    pub fn set_tip(&mut self, text: &str) {
        if text != self.tip {
            self.write_tip(text);
            unsafe { Shell_NotifyIconW(NIM_MODIFY, &self.data) };
        }
    }

    fn write_tip(&mut self, text: &str) {
        self.tip = text.to_owned();
        let buf = &mut self.data.szTip;
        let max = buf.len() - 1; // room for the terminating NUL
        buf.fill(0);
        for (dst, src) in buf.iter_mut().zip(text.encode_utf16().take(max)) {
            *dst = src;
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &self.data);
            DestroyIcon(self.icon);
        }
    }
}

fn make_hicon(size: u32) -> Option<HICON> {
    let rgba = fan_icon_rgba(size);
    // CreateIcon wants BGRA colour bits plus a 1 bpp AND mask (all zero: alpha decides).
    let bgra: Vec<u8> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[2], p[1], p[0], p[3]])
        .collect();
    let and_mask = vec![0u8; size.div_ceil(16) as usize * 2 * size as usize];
    let h = unsafe {
        CreateIcon(
            std::ptr::null_mut(),
            size as i32,
            size as i32,
            1,
            32,
            and_mask.as_ptr(),
            bgra.as_ptr(),
        )
    };
    (!h.is_null()).then_some(h)
}

/// The fan glyph from the UI (ring, five swept blades, hub) in amber, as straight-alpha RGBA,
/// rasterised with 4×4 supersampling. Used for both the tray and the window icon.
pub fn fan_icon_rgba(size: u32) -> Vec<u8> {
    const AMBER: [u8; 3] = [0xF2, 0xC9, 0x4C];
    const SS: u32 = 4;
    let r = 0.94f32;
    let blades: Vec<(f32, f32, f32, f32)> = (0..5)
        .map(|i| {
            let a = -TAU / 4.0 + i as f32 * TAU / 5.0;
            let tilt = a + 0.9;
            (a.cos() * r * 0.5, a.sin() * r * 0.5, tilt.cos(), tilt.sin())
        })
        .collect();
    let inside = |x: f32, y: f32| -> bool {
        let d = (x * x + y * y).sqrt();
        if d > r {
            return false;
        }
        if d >= r * 0.80 {
            return true; // ring
        }
        if d <= r * 0.10 {
            return false; // hole in the hub
        }
        if d <= r * 0.26 {
            return true; // hub
        }
        blades.iter().any(|&(cx, cy, c, s)| {
            let (dx, dy) = (x - cx, y - cy);
            let (u, v) = (dx * c + dy * s, -dx * s + dy * c);
            (u / (r * 0.42)).powi(2) + (v / (r * 0.19)).powi(2) <= 1.0
        })
    };
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = ((px * SS + sx) as f32 + 0.5) / (size * SS) as f32 * 2.0 - 1.0;
                    let y = ((py * SS + sy) as f32 + 0.5) / (size * SS) as f32 * 2.0 - 1.0;
                    hits += inside(x, y) as u32;
                }
            }
            let alpha = (hits * 255 / (SS * SS)) as u8;
            out.extend_from_slice(&[AMBER[0], AMBER[1], AMBER[2], alpha]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_shape() {
        let n = 32u32;
        let px = fan_icon_rgba(n);
        assert_eq!(px.len(), (n * n * 4) as usize);
        let alpha = |x: u32, y: u32| px[((y * n + x) * 4 + 3) as usize];
        assert_eq!(alpha(0, 0), 0, "corners are transparent");
        assert_eq!(alpha(n - 1, n - 1), 0);
        assert_eq!(alpha(n / 2, n / 2), 0, "hub has a hole");
        assert_eq!(alpha(n / 2 + 2, n / 2), 255, "hub is solid");
        assert_eq!(alpha(n / 2, 1), 255, "ring is solid");
    }

    #[test]
    fn tray_events_are_taken_once() {
        post(SHOW);
        post(QUIT);
        assert_eq!(take_events(), SHOW | QUIT);
        assert_eq!(take_events(), 0);
    }
}
