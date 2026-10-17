//! "Gunmetal" look, matched to lghub-lite: a grey chassis with raised buttons (dark outline,
//! light top bevel) and recessed black displays with mint/green phosphor text.
//!
//! The embedded fonts are trimmed to Latin, so arrows, triangles and dots are drawn with the
//! painter (see [`Icon`]) instead of being typed as text.

use std::f32::consts::TAU;
use std::sync::Arc;

use eframe::egui::{
    self, Align2, Color32, FontData, FontDefinitions, FontFamily, FontId, Painter, Pos2, Rect, Response, Sense, Stroke,
    StrokeKind, Vec2, epaint, pos2, vec2,
};

// Chassis.
pub const WIN: Color32 = Color32::from_rgb(0x2C, 0x2F, 0x35);
pub const OUTLINE: Color32 = Color32::from_rgb(0x16, 0x18, 0x1B);
pub const BEVEL: Color32 = Color32::from_rgb(0x4A, 0x4E, 0x57);
pub const RAISED: Color32 = Color32::from_rgb(0x38, 0x3C, 0x43);
pub const RAISED_HOT: Color32 = Color32::from_rgb(0x41, 0x45, 0x4D);
pub const TEXT: Color32 = Color32::from_rgb(0xD6, 0xD9, 0xDE);
pub const LABEL: Color32 = Color32::from_rgb(0xB4, 0xB8, 0xBD);
pub const HEADER: Color32 = Color32::from_rgb(0xA3, 0xA6, 0xAC);
// Displays.
pub const DISPLAY: Color32 = Color32::from_rgb(0x0A, 0x0D, 0x0B);
pub const ROW_SEL: Color32 = OUTLINE;
pub const MINT: Color32 = Color32::from_rgb(0xBF, 0xF5, 0xCF);
pub const DIM: Color32 = Color32::from_rgb(0x2F, 0x5A, 0x3E);
// Signals.
pub const AMBER: Color32 = Color32::from_rgb(0xF2, 0xC9, 0x4C);
pub const GREEN: Color32 = Color32::from_rgb(0x3D, 0xDC, 0x84);
pub const RED: Color32 = Color32::from_rgb(0xFF, 0x5A, 0x4D);

const SANS_MED: &str = "sans-medium";
const SANS_SEMI: &str = "sans-semibold";
const MONO_SEMI: &str = "mono-semibold";
const GROTESK: &str = "grotesk";

pub fn sans(size: f32) -> FontId {
    FontId::proportional(size)
}
pub fn sans_med(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SANS_MED.into()))
}
pub fn sans_semi(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SANS_SEMI.into()))
}
pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}
pub fn mono_semi(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(MONO_SEMI.into()))
}
pub fn grotesk(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(GROTESK.into()))
}

/// Replace egui's default fonts with the bundled ones (see third_party/fonts).
fn fonts() -> FontDefinitions {
    let mut defs = FontDefinitions::empty();
    let mut add = |name: &str, bytes: &'static [u8], tweak: epaint::text::FontTweak, family: FontFamily| {
        defs.font_data
            .insert(name.to_owned(), Arc::new(FontData::from_static(bytes).tweak(tweak)));
        defs.families.entry(family).or_default().push(name.to_owned());
    };
    let plain = epaint::text::FontTweak::default;
    add(
        "LLSans-Regular",
        include_bytes!("../../third_party/fonts/LLSans-Regular.ttf"),
        plain(),
        FontFamily::Proportional,
    );
    add(
        "LLMono-Regular",
        include_bytes!("../../third_party/fonts/LLMono-Regular.ttf"),
        plain(),
        FontFamily::Monospace,
    );
    add(
        "LLSans-Medium",
        include_bytes!("../../third_party/fonts/LLSans-Medium.ttf"),
        plain(),
        FontFamily::Name(SANS_MED.into()),
    );
    add(
        "LLSans-SemiBold",
        include_bytes!("../../third_party/fonts/LLSans-SemiBold.ttf"),
        plain(),
        FontFamily::Name(SANS_SEMI.into()),
    );
    add(
        "LLMono-SemiBold",
        include_bytes!("../../third_party/fonts/LLMono-SemiBold.ttf"),
        plain(),
        FontFamily::Name(MONO_SEMI.into()),
    );
    let heavy = epaint::text::FontTweak {
        coords: epaint::text::VariationCoords::new([(b"wght", 600.0)]),
        ..Default::default()
    };
    add(
        "SpaceGrotesk",
        include_bytes!("../../third_party/fonts/SpaceGrotesk-wght.ttf"),
        heavy,
        FontFamily::Name(GROTESK.into()),
    );
    // Every family falls back to the sans, so any stray character still renders.
    for fam in defs.families.values_mut() {
        if fam.first().map(String::as_str) != Some("LLSans-Regular") {
            fam.push("LLSans-Regular".into());
        }
    }
    defs
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|s| {
        s.animation_time = 0.0;
        s.spacing.item_spacing = vec2(8.0, 8.0);
        s.spacing.button_padding = vec2(10.0, 5.0);
        s.spacing.interact_size.y = 26.0;
        s.text_styles.insert(egui::TextStyle::Body, sans(14.0));
        s.text_styles.insert(egui::TextStyle::Button, sans_med(13.0));
        s.text_styles.insert(egui::TextStyle::Small, sans(11.0));
        s.text_styles.insert(egui::TextStyle::Monospace, mono(13.0));
        let v = &mut s.visuals;
        v.dark_mode = true;
        v.override_text_color = None;
        v.panel_fill = WIN;
        v.window_fill = WIN;
        v.window_stroke = Stroke::new(1.0, OUTLINE);
        v.extreme_bg_color = DISPLAY;
        v.faint_bg_color = DISPLAY;
        v.hyperlink_color = MINT;
        v.selection.bg_fill = DIM;
        v.selection.stroke = Stroke::new(1.0, MINT);
        v.slider_trailing_fill = true;
        for (w, fill, line, fg) in [
            (&mut v.widgets.noninteractive, WIN, OUTLINE, TEXT),
            (&mut v.widgets.inactive, RAISED, OUTLINE, TEXT),
            (&mut v.widgets.hovered, RAISED_HOT, OUTLINE, TEXT),
            (&mut v.widgets.active, RAISED_HOT, DIM, MINT),
            (&mut v.widgets.open, RAISED_HOT, OUTLINE, TEXT),
        ] {
            w.bg_fill = fill;
            w.weak_bg_fill = fill;
            w.bg_stroke = Stroke::new(1.0, line);
            w.fg_stroke = Stroke::new(1.0, fg);
            w.corner_radius = egui::CornerRadius::same(6);
            w.expansion = 0.0;
        }
    });
}

// ------------------------------------------------------------------ surfaces

/// Recessed black display panel.
pub fn display<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    display_frame(10.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

pub fn display_frame(margin: f32) -> egui::Frame {
    egui::Frame::new()
        .fill(DISPLAY)
        .stroke(Stroke::new(1.0, OUTLINE))
        .corner_radius(10)
        .inner_margin(margin)
}

/// Raised chassis surface: dark outline, fill, and a light bevel along the top edge.
pub fn paint_raised(p: &Painter, rect: Rect, radius: f32, fill: Color32) {
    p.rect(rect, radius, fill, Stroke::new(1.0, OUTLINE), StrokeKind::Inside);
    let y = rect.top() + 1.5;
    p.line_segment(
        [pos2(rect.left() + radius, y), pos2(rect.right() - radius, y)],
        Stroke::new(1.0, BEVEL),
    );
}

// ------------------------------------------------------------------ icons

#[derive(Clone, Copy, PartialEq)]
pub enum Icon {
    Left,
    Right,
    Up,
    Down,
    Dot(Color32),
    Sliders,
    Minus,
    Plus,
    Close,
}

pub fn paint_icon(p: &Painter, c: Pos2, icon: Icon, color: Color32) {
    let tri = |pts: [Vec2; 3]| {
        p.add(epaint::PathShape::convex_polygon(
            pts.iter().map(|v| c + *v).collect(),
            color,
            Stroke::NONE,
        ));
    };
    let s = Stroke::new(1.3, color);
    match icon {
        Icon::Left => tri([vec2(-4.0, 0.0), vec2(3.0, -4.5), vec2(3.0, 4.5)]),
        Icon::Right => tri([vec2(4.0, 0.0), vec2(-3.0, 4.5), vec2(-3.0, -4.5)]),
        Icon::Up => tri([vec2(0.0, -2.0), vec2(3.0, 2.0), vec2(-3.0, 2.0)]),
        Icon::Down => tri([vec2(0.0, 2.0), vec2(-3.0, -2.0), vec2(3.0, -2.0)]),
        Icon::Dot(fill) => {
            p.circle(c, 2.6, fill, Stroke::new(1.0, OUTLINE));
        }
        Icon::Sliders => {
            for (dy, knob) in [(-3.0, 2.0), (3.0, -2.0)] {
                p.line_segment([c + vec2(-5.0, dy), c + vec2(5.0, dy)], s);
                p.circle_filled(c + vec2(knob, dy), 2.0, color);
            }
        }
        Icon::Minus => {
            p.line_segment([c + vec2(-3.0, 0.0), c + vec2(3.0, 0.0)], s);
        }
        Icon::Plus => {
            p.line_segment([c + vec2(-3.0, 0.0), c + vec2(3.0, 0.0)], s);
            p.line_segment([c + vec2(0.0, -3.0), c + vec2(0.0, 3.0)], s);
        }
        Icon::Close => {
            p.line_segment([c + vec2(-3.0, -3.0), c + vec2(3.0, 3.0)], s);
            p.line_segment([c + vec2(-3.0, 3.0), c + vec2(3.0, -3.0)], s);
        }
    }
}

/// A bare icon that reacts to clicks (e.g. − / + inside a display).
pub fn icon_hit(ui: &mut egui::Ui, icon: Icon, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(20.0, 22.0), Sense::click());
    let c = if resp.hovered() {
        color
    } else {
        color.gamma_multiply(0.8)
    };
    paint_icon(ui.painter(), rect.center(), icon, c);
    resp
}

// ------------------------------------------------------------------ controls

/// A chassis button. `sunk` draws it pressed-in (display style), like an open drawer toggle.
pub fn button(ui: &mut egui::Ui, lead: Option<Icon>, text: &str, trail: Option<Icon>, sunk: bool) -> Response {
    // Proportions measured from lghub-lite's toolbar.
    let galley = ui.painter().layout_no_wrap(text.to_owned(), sans_med(12.0), TEXT);
    let icon_w = |i: Option<Icon>| match i {
        None => 0.0,
        Some(Icon::Sliders) => 12.0,
        Some(_) => 8.0,
    };
    let gap = |i: Option<Icon>| if i.is_some() && !text.is_empty() { 6.0 } else { 0.0 };
    let pad = 9.0;
    let content_w = icon_w(lead) + gap(lead) + galley.size().x + gap(trail) + icon_w(trail);
    let (rect, resp) = ui.allocate_exact_size(vec2((content_w + pad * 2.0).max(31.0), 27.0), Sense::click());
    let p = ui.painter();
    if sunk {
        p.rect(rect, 7, DISPLAY, Stroke::new(1.0, OUTLINE), StrokeKind::Inside);
    } else {
        paint_raised(p, rect, 7.0, if resp.hovered() { RAISED_HOT } else { RAISED });
    }
    let mut x = rect.center().x - content_w / 2.0;
    let cy = rect.center().y;
    if let Some(i) = lead {
        paint_icon(p, pos2(x + icon_w(lead) / 2.0, cy), i, LABEL);
        x += icon_w(lead) + gap(lead);
    }
    let text_w = galley.size().x;
    p.galley(pos2(x, cy - galley.size().y / 2.0), galley, TEXT);
    x += text_w + gap(trail);
    if let Some(i) = trail {
        paint_icon(p, pos2(x + icon_w(trail) / 2.0, cy), i, LABEL);
    }
    resp
}

/// Option pill inside a display: green fill when selected, green outline otherwise.
pub fn pill(ui: &mut egui::Ui, selected: bool, text: &str) -> Response {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), sans_med(13.0), MINT);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 16.0, 26.0), Sense::click());
    let fill = if selected {
        DIM
    } else if resp.hovered() {
        OUTLINE
    } else {
        DISPLAY
    };
    ui.painter()
        .rect(rect, 5, fill, Stroke::new(1.0, DIM), StrokeKind::Inside);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, MINT);
    resp
}

/// Small outlined tag inside a display.
pub fn chip(ui: &mut egui::Ui, text: &str) {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), mono(11.0), MINT);
    let (rect, _) = ui.allocate_exact_size(galley.size() + vec2(14.0, 4.0), Sense::hover());
    ui.painter()
        .rect(rect, 4, Color32::TRANSPARENT, Stroke::new(1.0, DIM), StrokeKind::Inside);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, MINT);
}

/// "DEVICES ———" section title on the chassis.
pub fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(title).font(sans_semi(12.0)).color(HEADER));
        let r = ui.available_rect_before_wrap();
        let y = r.center().y;
        ui.painter()
            .line_segment([pos2(r.left(), y), pos2(r.right(), y)], Stroke::new(1.0, OUTLINE));
    });
}

/// Text with a soft glow, like a lit display segment.
pub fn glow_text(p: &Painter, pos: Pos2, anchor: Align2, text: &str, font: FontId, color: Color32) -> Rect {
    let halo = color.gamma_multiply(0.07);
    for (dx, dy) in [
        (-2.0, 0.0),
        (2.0, 0.0),
        (0.0, -2.0),
        (0.0, 2.0),
        (-1.5, -1.5),
        (1.5, 1.5),
        (-1.5, 1.5),
        (1.5, -1.5),
    ] {
        p.text(pos + vec2(dx, dy), anchor, text, font.clone(), halo);
    }
    p.text(pos, anchor, text, font, color)
}

/// The title-bar status light: solid core and a faint halo.
pub fn status_light(p: &Painter, c: Pos2, color: Color32) {
    for (r, a) in [(6.0, 0.10), (5.0, 0.22), (4.3, 0.38)] {
        p.circle_filled(c, r, color.gamma_multiply(a));
    }
    p.circle_filled(c, 3.6, color);
}

/// Horizontal level bar, `frac` in 0..=1.
pub fn hbar(ui: &mut egui::Ui, width: f32, frac: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 4.0), Sense::hover());
    let w = rect.width() * frac.clamp(0.0, 1.0);
    if w > 0.5 {
        ui.painter()
            .rect_filled(Rect::from_min_size(rect.min, vec2(w.max(5.0), rect.height())), 1, color);
    }
}

/// Vertical segmented gauge in its own recessed box. `marker` draws a red line at that fraction.
pub fn seg_meter(ui: &mut egui::Ui, size: Vec2, frac: f32, marker: Option<f32>) {
    const SEGS: usize = 12;
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter();
    p.rect(rect, 8, DISPLAY, Stroke::new(1.0, OUTLINE), StrokeKind::Inside);
    let inner = rect.shrink2(vec2(5.0, 6.0));
    let gap = 3.0;
    let h = (inner.height() - gap * (SEGS - 1) as f32) / SEGS as f32;
    let lit = (frac.clamp(0.0, 1.0) * SEGS as f32).round() as usize;
    for i in 0..SEGS {
        let y = inner.bottom() - (i as f32 + 1.0) * h - i as f32 * gap;
        let seg = Rect::from_min_size(pos2(inner.left(), y), vec2(inner.width(), h));
        if i < lit {
            p.rect_filled(seg.expand(1.0), 3, AMBER.gamma_multiply(0.15));
            p.rect_filled(seg, 2, AMBER);
        } else {
            p.rect(
                seg,
                2,
                Color32::TRANSPARENT,
                Stroke::new(1.0, OUTLINE),
                StrokeKind::Inside,
            );
        }
    }
    if let Some(m) = marker {
        let y = inner.bottom() - m.clamp(0.0, 1.0) * inner.height();
        p.line_segment(
            [pos2(rect.left() + 2.0, y), pos2(rect.right() - 2.0, y)],
            Stroke::new(2.0, RED),
        );
    }
}

/// Fan glyph: ring, hub and five swept blades, rotated by `angle` (radians).
pub fn fan_icon(painter: &Painter, center: Pos2, r: f32, angle: f32, color: Color32) {
    painter.circle_stroke(center, r, Stroke::new((r * 0.08).max(1.0), color.gamma_multiply(0.45)));
    for i in 0..5 {
        let a = angle + i as f32 * TAU / 5.0;
        // Each blade is a small ellipse pushed out from the hub and tilted: a swept look
        // that stays a convex polygon.
        let (dir, tilt) = (Vec2::angled(a), a + 0.9);
        let c = center + dir * r * 0.5;
        let (ax, ay) = (r * 0.40, r * 0.17);
        let (s, k) = tilt.sin_cos();
        let pts: Vec<Pos2> = (0..12)
            .map(|j| {
                let t = j as f32 / 12.0 * TAU;
                let (x, y) = (ax * t.cos(), ay * t.sin());
                c + vec2(x * k - y * s, x * s + y * k)
            })
            .collect();
        painter.add(epaint::PathShape::convex_polygon(pts, color, Stroke::NONE));
    }
    painter.circle_filled(center, r * 0.22, color);
    painter.circle_filled(center, r * 0.09, DISPLAY);
}

/// Allocate space for a fan glyph and draw it.
pub fn fan_widget(ui: &mut egui::Ui, size: f32, angle: f32, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    fan_icon(ui.painter(), rect.center(), size * 0.47, angle, color);
    resp
}

/// Visual rotation speed (revolutions/second) for a real fan speed. Real fans turn far too
/// fast to show, so this compresses the range while keeping faster = faster.
pub fn visual_rps(rpm: Option<f32>, duty: Option<f32>) -> f32 {
    match (rpm, duty) {
        (Some(r), _) => (r / 1000.0).clamp(0.0, 2.6),
        (None, Some(d)) => (d / 40.0).clamp(0.0, 2.6),
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faster_fans_spin_faster_but_capped() {
        assert_eq!(visual_rps(Some(0.0), Some(50.0)), 0.0);
        assert!(visual_rps(Some(325.0), None) < visual_rps(Some(2350.0), None));
        assert_eq!(visual_rps(Some(9000.0), None), 2.6);
        assert_eq!(visual_rps(None, Some(40.0)), 1.0);
        assert_eq!(visual_rps(None, None), 0.0);
    }

    #[test]
    fn bundled_fonts_load() {
        let ctx = egui::Context::default();
        ctx.set_fonts(fonts());
        let out = ctx.run_ui(Default::default(), |ui| {
            for f in [
                sans(14.0),
                sans_med(13.0),
                sans_semi(12.0),
                mono(13.0),
                mono_semi(13.0),
                grotesk(60.0),
            ] {
                ui.label(egui::RichText::new("Fan 42% · 1350 rpm 45.5°").font(f));
            }
        });
        out.drop_without_applying_deltas();
    }
}
