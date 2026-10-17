//! Interactive fan-curve graph drawn directly with the egui painter (no plotting crate).
//! Drag points to move them, double-click empty space to add one, right-click a point to remove it.

use eframe::egui::{self, Align2, Pos2, Rect, Sense, Stroke, Vec2, pos2, vec2};

use crate::config::Curve;

/// Temperature axis: (from, to, grid step). Curves that stay at or below 60 °C (an AIO's
/// liquid temperature) get a zoomed-in axis so their points aren't squeezed together.
fn axis(curve: &Curve) -> (f32, f32, f32) {
    if curve.points.iter().all(|p| p.0 <= 60.0) {
        (20.0, 60.0, 5.0)
    } else {
        (20.0, 110.0, 10.0)
    }
}

const PAD: Vec2 = vec2(36.0, 18.0);

/// Draws the editor. Returns true if the curve changed. `live` is (temperature, output %).
pub fn show(ui: &mut egui::Ui, curve: &mut Curve, live: Option<(f32, f32)>) -> bool {
    let (t_min, t_max, step) = axis(curve);
    let width = ui.available_width().max(260.0);
    let (resp, painter) = ui.allocate_painter(vec2(width, 240.0), Sense::click());
    let plot = Rect::from_min_max(resp.rect.min + vec2(PAD.x, 6.0), resp.rect.max - vec2(8.0, PAD.y));
    let fg = super::theme::MINT;
    let weak = super::theme::DIM;
    let grid = Stroke::new(1.0, super::theme::OUTLINE);
    let accent = super::theme::AMBER;

    let to_screen = |t: f32, p: f32| {
        pos2(
            plot.left() + (t - t_min) / (t_max - t_min) * plot.width(),
            plot.bottom() - p / 100.0 * plot.height(),
        )
    };
    let from_screen = |pos: Pos2| {
        (
            t_min + (pos.x - plot.left()) / plot.width() * (t_max - t_min),
            (plot.bottom() - pos.y) / plot.height() * 100.0,
        )
    };

    // Grid and axis labels.
    let font = super::theme::mono(10.0);
    for i in 0..=10 {
        let p = i as f32 * 10.0;
        let y = to_screen(t_min, p).y;
        painter.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], grid);
        if i % 2 == 0 {
            painter.text(
                pos2(plot.left() - 4.0, y),
                Align2::RIGHT_CENTER,
                format!("{p:.0}%"),
                font.clone(),
                weak,
            );
        }
    }
    let mut t = t_min;
    while t <= t_max {
        let x = to_screen(t, 0.0).x;
        painter.line_segment([pos2(x, plot.top()), pos2(x, plot.bottom())], grid);
        painter.text(
            pos2(x, plot.bottom() + 2.0),
            Align2::CENTER_TOP,
            format!("{t:.0}°"),
            font.clone(),
            weak,
        );
        t += step;
    }

    // Curve line, flat beyond the end points.
    let first = curve.points[0];
    let last = curve.points[curve.points.len() - 1];
    let mut line = vec![to_screen(t_min, first.1)];
    line.extend(curve.points.iter().map(|&(t, p)| to_screen(t.clamp(t_min, t_max), p)));
    line.push(to_screen(t_max, last.1));
    painter.add(egui::Shape::line(line, Stroke::new(2.0, accent)));

    // Live operating point.
    if let Some((t, p)) = live {
        let x = to_screen(t.clamp(t_min, t_max), 0.0).x;
        painter.line_segment(
            [pos2(x, plot.top()), pos2(x, plot.bottom())],
            Stroke::new(1.0, super::theme::GREEN),
        );
        painter.circle_filled(to_screen(t.clamp(t_min, t_max), p), 4.0, super::theme::GREEN);
        painter.text(
            pos2(x + 4.0, plot.top() + 2.0),
            Align2::LEFT_TOP,
            format!("{t:.1}° → {p:.0}%"),
            font.clone(),
            fg,
        );
    }

    // Point handles. While dragging, a point is boxed between its neighbours so the
    // ordering and monotonic invariants hold without re-sorting mid-drag.
    let mut changed = false;
    let mut remove = None;
    let n = curve.points.len();
    for i in 0..n {
        let (t, p) = curve.points[i];
        let center = to_screen(t.clamp(t_min, t_max), p);
        let r = ui.interact(
            Rect::from_center_size(center, Vec2::splat(14.0)),
            resp.id.with(i),
            Sense::click_and_drag(),
        );
        if r.dragged()
            && let Some(pos) = r.interact_pointer_pos()
        {
            let (nt, np) = from_screen(pos);
            let lo_t = if i > 0 { curve.points[i - 1].0 + 1.0 } else { 0.0 };
            let hi_t = if i + 1 < n { curve.points[i + 1].0 - 1.0 } else { 120.0 };
            let lo_p = if i > 0 { curve.points[i - 1].1 } else { 0.0 };
            let hi_p = if i + 1 < n { curve.points[i + 1].1 } else { 100.0 };
            curve.points[i] = (nt.clamp(lo_t, hi_t.max(lo_t)).round(), np.clamp(lo_p, hi_p).round());
            changed = true;
        }
        if r.secondary_clicked() && n > 2 {
            remove = Some(i);
        }
        let active = r.hovered() || r.dragged();
        painter.circle(center, if active { 6.0 } else { 4.5 }, accent, Stroke::new(1.5, fg));
        if active {
            let (t, p) = curve.points[i];
            painter.text(
                center + vec2(8.0, -8.0),
                Align2::LEFT_BOTTOM,
                format!("{t:.0}°, {p:.0}%"),
                font.clone(),
                fg,
            );
        }
    }
    if let Some(i) = remove {
        curve.points.remove(i);
        changed = true;
    }
    if resp.double_clicked()
        && let Some(pos) = resp.interact_pointer_pos()
        && plot.contains(pos)
    {
        let (t, p) = from_screen(pos);
        curve.points.push((t.round(), p.round()));
        changed = true;
    }
    if changed {
        curve.normalize();
    }
    resp.on_hover_text("Drag points to move · double-click to add · right-click a point to remove");
    changed
}
