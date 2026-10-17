//! Click-to-rename labels for fans and sensors.

use std::collections::BTreeMap;

use eframe::egui::text::{CCursor, CCursorRange};
use eframe::egui::{self, Key, RichText};

/// An in-progress rename.
pub struct Rename {
    id: String,
    buf: String,
    /// Focus (and select-all) is requested once, on the first frame only. Requesting it every
    /// frame would steal focus back and the edit could never be committed.
    start: bool,
}

/// Shows `labels[id]` (or `default`). Click to edit; Enter or clicking elsewhere saves,
/// Esc cancels, an empty name resets to the default. Returns true when `labels` changed.
pub fn name_label(
    ui: &mut egui::Ui,
    state: &mut Option<Rename>,
    labels: &mut BTreeMap<String, String>,
    id: &str,
    default: &str,
) -> bool {
    if let Some(r) = state.as_mut().filter(|r| r.id == id) {
        let resp = ui.add(
            egui::TextEdit::singleline(&mut r.buf)
                .font(super::theme::sans(13.0))
                .desired_width(150.0),
        );
        if r.start {
            r.start = false;
            resp.request_focus();
            if let Some(mut te) = egui::TextEdit::load_state(ui.ctx(), resp.id) {
                let end = CCursor::new(r.buf.chars().count());
                te.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(0), end)));
                te.store(ui.ctx(), resp.id);
            }
            return false;
        }
        if resp.lost_focus() {
            let cancelled = ui.input(|i| i.key_pressed(Key::Escape));
            let name = r.buf.trim().to_owned();
            *state = None;
            if cancelled {
                return false;
            }
            let old = labels.get(id).cloned();
            if name.is_empty() || name == default {
                labels.remove(id);
            } else {
                labels.insert(id.to_owned(), name);
            }
            return labels.get(id) != old.as_ref();
        }
        return false;
    }

    let label = labels.get(id).map_or(default, String::as_str).to_owned();
    let resp = ui
        .add(
            egui::Label::new(
                RichText::new(&label)
                    .font(super::theme::sans(13.0))
                    .color(super::theme::MINT),
            )
            .sense(egui::Sense::click()),
        )
        .on_hover_cursor(egui::CursorIcon::Text)
        .on_hover_text(format!("Click to rename\n{id}"));
    if resp.clicked() {
        *state = Some(Rename {
            id: id.to_owned(),
            buf: label,
            start: true,
        });
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, Modifiers, Pos2, RawInput, Rect, vec2};

    struct Harness {
        ctx: egui::Context,
        state: Option<Rename>,
        labels: BTreeMap<String, String>,
        rect: Rect,
        changed: bool,
    }

    impl Harness {
        fn new() -> Self {
            Harness {
                ctx: egui::Context::default(),
                state: None,
                labels: BTreeMap::new(),
                rect: Rect::NOTHING,
                changed: false,
            }
        }

        fn frame(&mut self, events: Vec<Event>) {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0))),
                events,
                ..Default::default()
            };
            let (state, labels) = (&mut self.state, &mut self.labels);
            let mut rect = Rect::NOTHING;
            let mut changed = false;
            let out = self.ctx.run_ui(input, |ui| {
                changed |= name_label(ui, state, labels, "nct/pwm2", "Fan #2");
                rect = ui.min_rect();
            });
            out.drop_without_applying_deltas();
            self.rect = rect;
            self.changed |= changed;
            // Let egui settle (focus changes apply on the following pass).
            let out = self.ctx.run_ui(RawInput::default(), |ui| {
                self.changed |= name_label(ui, &mut self.state, &mut self.labels, "nct/pwm2", "Fan #2");
            });
            out.drop_without_applying_deltas();
        }

        fn click(&mut self, pos: Pos2) {
            let btn = |pressed| Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            };
            self.frame(vec![Event::PointerMoved(pos), btn(true)]);
            self.frame(vec![btn(false)]);
        }

        fn key(&mut self, key: Key) {
            let k = |pressed| Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: Modifiers::NONE,
            };
            self.frame(vec![k(true), k(false)]);
        }

        fn start_rename(&mut self) {
            self.frame(vec![]);
            let center = self.rect.center();
            self.click(center);
            assert!(self.state.is_some(), "clicking the name should start editing");
        }
    }

    #[test]
    fn rename_commits_on_enter() {
        let mut h = Harness::new();
        h.start_rename();
        h.frame(vec![Event::Text("CPU fan".into())]);
        h.key(Key::Enter);
        assert!(h.state.is_none());
        assert!(h.changed);
        assert_eq!(h.labels.get("nct/pwm2").map(String::as_str), Some("CPU fan"));
    }

    #[test]
    fn rename_commits_on_click_elsewhere() {
        let mut h = Harness::new();
        h.start_rename();
        h.frame(vec![Event::Text("Hub".into())]);
        h.click(Pos2::new(390.0, 190.0));
        assert_eq!(h.labels.get("nct/pwm2").map(String::as_str), Some("Hub"));
    }

    #[test]
    fn escape_cancels_and_empty_resets() {
        let mut h = Harness::new();
        h.labels.insert("nct/pwm2".into(), "Old".into());
        h.start_rename();
        h.frame(vec![Event::Text("New".into())]);
        h.key(Key::Escape);
        assert_eq!(h.labels.get("nct/pwm2").map(String::as_str), Some("Old"));

        h.start_rename();
        h.key(Key::Backspace); // whole name is selected, so this clears it
        h.key(Key::Enter);
        assert_eq!(h.labels.get("nct/pwm2"), None);
    }
}
