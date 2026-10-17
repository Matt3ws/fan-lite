//! Custom window chrome, matched to lghub-lite: the native title bar is off and we draw a
//! gunmetal frame with a top bevel, a status light and title, and two round buttons
//! (minimize, close). Like lghub-lite the window has a fixed width and its height follows
//! the content ([`fit_height`]). Geometry is in points, measured from lghub-lite.

use eframe::egui::{self, Align2, Rect, Sense, Stroke, StrokeKind, UiBuilder, ViewportCommand, pos2, vec2};

use super::theme::{self, AMBER, BEVEL, OUTLINE, RAISED, RED, TEXT, WIN};

/// Window width, as lghub-lite.
pub const WIDTH: f32 = 420.0;
/// Content starts this far below the window top.
const TITLE_H: f32 = 40.0;
/// Content inset from the left/right/bottom edges.
const SIDE: f32 = 11.0;
const RADIUS: f32 = 10.0;
/// Title-bar row centre, status light x, title text x.
const ROW_Y: f32 = 21.5;
const LIGHT_X: f32 = 18.5;
const TITLE_X: f32 = 31.0;
/// Round buttons: radius (the 1 pt outline adds half a point), spacing between centres,
/// right button centre from the right edge.
const BTN_R: f32 = 5.0;
const BTN_GAP: f32 = 18.0;
const BTN_RIGHT: f32 = 21.5;

/// Draws the frame and title bar, then calls `content` inside the remaining area.
/// `light` is the status light colour; `title_extra` adds widgets after the title text.
pub fn window(
    ui: &mut egui::Ui,
    light: egui::Color32,
    title_extra: impl FnOnce(&mut egui::Ui),
    content: impl FnOnce(&mut egui::Ui),
) {
    egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
        let rect = ui.max_rect();
        let p = ui.painter();
        p.rect(rect, RADIUS, WIN, Stroke::new(1.0, OUTLINE), StrokeKind::Inside);
        // Top bevel: a rounded outline one pixel in, clipped to the top band, so the
        // highlight follows the corner curves like lghub-lite's instead of stopping short.
        let band = Rect::from_min_size(rect.min, vec2(rect.width(), RADIUS * 0.8));
        p.with_clip_rect(band).rect_stroke(
            rect.shrink(1.0),
            RADIUS - 1.0,
            Stroke::new(1.0, BEVEL),
            StrokeKind::Inside,
        );

        let bar = Rect::from_min_size(rect.min, vec2(rect.width(), TITLE_H));
        // Registered before the title widgets so those stay clickable on top of it.
        let drag = ui.interact(bar, ui.scope_id().with("title_bar"), Sense::drag());
        if drag.drag_started_by(egui::PointerButton::Primary) {
            ui.send_viewport_cmd(ViewportCommand::StartDrag);
        }

        theme::status_light(ui.painter(), rect.min + vec2(LIGHT_X, ROW_Y), light);
        let title = ui.painter().text(
            rect.min + vec2(TITLE_X, ROW_Y),
            Align2::LEFT_CENTER,
            "FanLite",
            theme::sans_semi(13.0),
            TEXT,
        );

        let close_c = pos2(rect.right() - BTN_RIGHT, rect.top() + ROW_Y);
        let min_c = close_c - vec2(BTN_GAP, 0.0);
        if round_button(ui, min_c, "minimize", false).clicked() {
            ui.send_viewport_cmd(ViewportCommand::Minimized(true));
        }
        if round_button(ui, close_c, "close", true).clicked() {
            ui.send_viewport_cmd(ViewportCommand::Close);
        }

        let extra = Rect::from_min_max(
            pos2(title.right() + 10.0, rect.top() + ROW_Y - 11.0),
            pos2(min_c.x - BTN_R - 10.0, rect.top() + ROW_Y + 11.0),
        );
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(extra)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            title_extra,
        );

        let body = Rect::from_min_max(
            pos2(rect.left() + SIDE, rect.top() + TITLE_H),
            rect.max - vec2(SIDE, SIDE),
        );
        ui.scope_builder(UiBuilder::new().max_rect(body), content);
    });
}

/// Make the native window match lghub-lite's edges:
///
/// * Windows 11 draws a 1 px light border around every top-level window, outside our
///   painted frame. Turn it off.
/// * winit's "undecorated shadow" (which egui always enables for borderless windows) moves
///   the client area 1 px down, leaving a dark strip along the top edge. Answer
///   `WM_NCCALCSIZE` ourselves so the client area is the whole window again.
/// * Tray icon clicks arrive at the same window procedure (see `tray::handle_message`).
///
/// Returns the window handle, for the tray icon.
pub fn native_window_tweaks(cc: &eframe::CreationContext<'_>) -> Option<windows_sys::Win32::Foundation::HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Graphics::Dwm::{DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DwmSetWindowAttribute};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GWLP_WNDPROC, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetWindowLongPtrW,
        SetWindowPos,
    };
    let Ok(handle) = cc.window_handle() else { return None };
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return None;
    };
    let hwnd = h.hwnd.get() as windows_sys::Win32::Foundation::HWND;
    unsafe {
        let none: u32 = DWMWA_COLOR_NONE;
        // Fails harmlessly on Windows 10, which has no such border.
        DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR as u32, &none as *const u32 as *const _, 4);

        let old = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, wndproc as *const () as isize);
        OLD_WNDPROC.store(old, std::sync::atomic::Ordering::Relaxed);
        // Ask Windows to recompute the frame with the new handler.
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    Some(hwnd)
}

/// winit's window procedure, which we forward every other message to.
static OLD_WNDPROC: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

unsafe extern "system" fn wndproc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::{CallWindowProcW, IsZoomed, WM_NCCALCSIZE, WNDPROC};
    if let Some(r) = super::tray::handle_message(hwnd, msg, wparam, lparam) {
        return r;
    }
    // Client area = whole window rect (returning 0 leaves the proposed rect untouched).
    // Maximized windows still go to winit, which clamps them to the monitor's work area.
    if msg == WM_NCCALCSIZE && wparam != 0 && unsafe { IsZoomed(hwnd) } == 0 {
        return 0;
    }
    let old: WNDPROC = unsafe { std::mem::transmute(OLD_WNDPROC.load(std::sync::atomic::Ordering::Relaxed)) };
    unsafe { CallWindowProcW(old, hwnd, msg, wparam, lparam) }
}

/// Resize the window so it fits `content_h` (the scrolled content's height), like
/// lghub-lite does. Capped to the screen; beyond that the content scrolls.
pub fn fit_height(ctx: &egui::Context, content_h: f32) {
    let (current, monitor) = ctx.input(|i| (i.viewport().inner_rect.map(|r| r.height()), i.viewport().monitor_size));
    let max = monitor.map_or(1000.0, |m| m.y - 80.0);
    let want = (TITLE_H + content_h + SIDE).ceil().clamp(160.0, max);
    if current.is_none_or(|h| (h - want).abs() > 0.5) {
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(vec2(WIDTH, want)));
    }
}

/// Plain dark circle with an outline. On hover it just changes colour (close red,
/// minimize amber) and shows the hand cursor; no symbol, no tooltip.
fn round_button(ui: &mut egui::Ui, c: egui::Pos2, id: &str, close: bool) -> egui::Response {
    let rect = Rect::from_center_size(c, vec2(BTN_R * 2.0 + 4.0, BTN_R * 2.0 + 4.0));
    let resp = ui
        .interact(rect, ui.scope_id().with(id), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let fill = match (close, resp.hovered()) {
        (true, true) => RED,
        (false, true) => AMBER,
        _ => RAISED,
    };
    ui.painter().circle(c, BTN_R, fill, Stroke::new(1.0, OUTLINE));
    resp
}
