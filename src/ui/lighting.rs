//! The Lighting view: one synced effect for every RGB zone, per-zone overrides, and
//! "Save to device".

use eframe::egui::{self, Color32, Layout, RichText, Sense, UiBuilder, vec2};

use super::theme::{self, AMBER, DIM, GREEN, Icon, LABEL, RED, TEXT, display, pill};
use super::{App, View, dim, mint};
use crate::config::ZoneLight;
use crate::engine::{LightInfo, Snapshot};
use crate::rgb::{Effect, Kind, Rgb, Speed};

fn key(ui: &mut egui::Ui, t: &str) {
    ui.add_sized(vec2(62.0, 26.0), egui::Label::new(dim(t, theme::sans(13.0))));
}

fn color32(c: Rgb) -> Color32 {
    Color32::from_rgb(c.0, c.1, c.2)
}

/// Colour dot for a zone: its colour, or a three-colour dot for the multi-colour effects.
fn swatch(ui: &mut egui::Ui, e: Option<&Effect>) {
    let (r, _) = ui.allocate_exact_size(vec2(14.0, 18.0), Sense::hover());
    let p = ui.painter();
    let c = r.center();
    match e.map(|e| (e, e.swatch())) {
        None => {
            p.circle_stroke(c, 4.0, (1.0, DIM));
        }
        Some((_, Some(col))) => {
            p.circle(c, 4.5, color32(col), (1.0, DIM));
        }
        Some((_, None)) => {
            for (i, col) in [RED, GREEN, Color32::from_rgb(0x4C, 0x7D, 0xFF)]
                .into_iter()
                .enumerate()
            {
                let a = i as f32 * std::f32::consts::TAU / 3.0 - std::f32::consts::FRAC_PI_2;
                p.circle_filled(c + vec2(a.cos(), a.sin()) * 2.6, 2.2, col);
            }
        }
    }
}

/// Edit an effect. `kinds` = what the zone supports (others are marked as substituted).
/// Returns true if it changed.
fn effect_editor(ui: &mut egui::Ui, e: &mut Effect, kinds: Option<&[Kind]>) -> bool {
    let before = *e;
    ui.horizontal(|ui| {
        key(ui, "Effect");
        ui.horizontal_wrapped(|ui| {
            for k in Kind::ALL {
                let r = pill(ui, e.kind == k, k.name());
                let r = match kinds.map(|ks| Effect { kind: k, ..*e }.fit(ks)) {
                    Some(Some(f)) if f.kind != k => {
                        r.on_hover_text(format!("Not supported here; shows {}", f.kind.name()))
                    }
                    Some(None) => r.on_hover_text("Not supported here"),
                    _ => r,
                };
                if r.clicked() {
                    e.kind = k;
                }
            }
        });
    });
    let n = e.kind.colors();
    if n > 0 {
        ui.horizontal(|ui| {
            key(ui, if n == 1 { "Color" } else { "Colors" });
            for c in e.colors.iter_mut().take(n) {
                let mut rgb = [c.0, c.1, c.2];
                if egui::color_picker::color_edit_button_srgb(ui, &mut rgb).changed() {
                    *c = Rgb(rgb[0], rgb[1], rgb[2]);
                }
            }
            ui.label(dim(e.colors[0].hex(), theme::mono(12.0)));
        });
    }
    if !matches!(e.kind, Kind::Off | Kind::Static) {
        ui.horizontal(|ui| {
            key(ui, "Speed");
            for s in Speed::ALL {
                if pill(ui, e.speed == s, s.name()).clicked() {
                    e.speed = s;
                }
            }
        });
    }
    if e.kind != Kind::Off {
        ui.horizontal(|ui| {
            key(ui, "Bright");
            ui.add(egui::Slider::new(&mut e.brightness, 0..=100).suffix("%"));
        });
    }
    *e != before
}

impl App {
    pub(super) fn lighting_view(&mut self, ui: &mut egui::Ui, snap: &Snapshot) -> bool {
        let mut changed = false;
        let any_applied = snap.lights.iter().any(|l| l.applied.is_some());
        ui.horizontal(|ui| {
            if theme::button(ui, Some(Icon::Left), "Back", None, false).clicked() {
                self.view = View::Main;
            }
            ui.label(RichText::new("Lighting").font(theme::sans_semi(14.0)).color(TEXT));
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                if any_applied
                    && theme::button(ui, None, "Save to device", None, false)
                        .on_hover_text(
                            "Store the current lighting in the devices, so it is also there after a restart \
                             without fan-lite. Writes to their flash memory, so only when you're happy with it.",
                        )
                        .clicked()
                {
                    self.engine.save_lights();
                    self.light_note = Some("saving…".into());
                }
            });
        });

        // Sync: one effect for every zone without its own setting.
        theme::display_frame(12.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = vec2(6.0, 8.0);
            ui.horizontal(|ui| {
                key(ui, "Sync all");
                let on = self.cfg.lighting.sync.is_some();
                if pill(ui, on, "On").clicked() && !on {
                    self.cfg.lighting.sync = Some(Effect::default());
                    changed = true;
                }
                if pill(ui, !on, "Off")
                    .on_hover_text("fan-lite leaves lights without their own effect as they are")
                    .clicked()
                    && on
                {
                    self.cfg.lighting.sync = None;
                    changed = true;
                }
            });
            if let Some(e) = &mut self.cfg.lighting.sync {
                changed |= effect_editor(ui, e, None);
            } else {
                ui.label(dim(
                    "Off: fan-lite doesn't touch any light unless you give it its own effect below.",
                    theme::sans(12.0),
                ));
            }
        });

        theme::section(ui, "ZONES");
        if snap.inv.lights.is_empty() {
            display(ui, |ui| {
                let msg = if snap.ready {
                    "NO RGB DEVICES FOUND · SEE NOTICES"
                } else {
                    "DETECTING HARDWARE…"
                };
                ui.label(mint(msg, theme::mono(12.0)));
            });
        } else {
            changed |= self.zone_list(ui, snap);
        }
        if let Some(n) = &self.light_note {
            ui.label(RichText::new(n).font(theme::sans(12.0)).color(LABEL));
        }
        changed
    }

    fn zone_list(&mut self, ui: &mut egui::Ui, snap: &Snapshot) -> bool {
        let mut changed = false;
        let mut open: Option<LightInfo> = None;
        Self::list_panel(ui, |ui| {
            for (k, z) in snap.inv.lights.iter().enumerate() {
                let view = snap.lights.get(k).cloned().unwrap_or_default();
                let setting = self.cfg.lighting.zones.get(&z.id);
                let sel = self.light_sel.as_deref() == Some(z.id.as_str());
                let resp = ui
                    .scope_builder(UiBuilder::new().sense(Sense::click()), |ui| {
                        Self::row_frame(sel).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                swatch(ui, view.applied.as_ref());
                                let name = self.cfg.label(&z.id, &z.label).to_owned();
                                ui.add(
                                    egui::Label::new(mint(name, theme::sans(13.0)))
                                        .truncate()
                                        .selectable(false),
                                );
                                ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                                    let (text, color) = match (&view.error, setting) {
                                        (Some(_), _) => ("error".to_owned(), RED),
                                        (None, Some(ZoneLight::Leave)) => ("left alone".to_owned(), LABEL),
                                        (None, Some(ZoneLight::Own(e))) => (e.to_string(), AMBER),
                                        (None, None) => match &self.cfg.lighting.sync {
                                            Some(_) => ("sync".to_owned(), GREEN),
                                            None => ("left alone".to_owned(), LABEL),
                                        },
                                    };
                                    ui.label(RichText::new(text).font(theme::mono_semi(12.0)).color(color));
                                    if z.leds > 0 {
                                        ui.label(dim(format!("{} LEDs", z.leds), theme::mono(12.0)));
                                    }
                                });
                            });
                        });
                    })
                    .response;
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.light_sel = if sel { None } else { Some(z.id.clone()) };
                }
                if sel {
                    open = Some(z.clone());
                }
            }
        });
        if let Some(z) = open {
            changed |= self.zone_drawer(ui, snap, &z);
        }
        changed
    }

    /// Settings for one zone: follow Sync, its own effect, or leave it alone.
    fn zone_drawer(&mut self, ui: &mut egui::Ui, snap: &Snapshot, z: &LightInfo) -> bool {
        let mut changed = false;
        let k = snap.inv.lights.iter().position(|l| l.id == z.id);
        let error = k.and_then(|k| snap.lights.get(k)).and_then(|v| v.error.clone());
        theme::display_frame(12.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = vec2(6.0, 8.0);
            ui.horizontal(|ui| {
                key(ui, "Name");
                changed |= self.name_label(ui, &z.id, &z.label);
                ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(dim(z.backend, theme::sans(12.0)));
                });
            });
            let cur = self.cfg.lighting.zones.get(&z.id).cloned();
            ui.horizontal(|ui| {
                key(ui, "Mode");
                if pill(ui, cur.is_none(), "Sync").clicked() && cur.is_some() {
                    self.cfg.lighting.zones.remove(&z.id);
                    changed = true;
                }
                if pill(ui, matches!(cur, Some(ZoneLight::Own(_))), "Own").clicked()
                    && !matches!(cur, Some(ZoneLight::Own(_)))
                {
                    let start = self.cfg.lighting.sync.unwrap_or_default();
                    self.cfg.lighting.zones.insert(z.id.clone(), ZoneLight::Own(start));
                    changed = true;
                }
                if pill(ui, cur == Some(ZoneLight::Leave), "Leave alone")
                    .on_hover_text("fan-lite never writes to this zone")
                    .clicked()
                    && cur != Some(ZoneLight::Leave)
                {
                    self.cfg.lighting.zones.insert(z.id.clone(), ZoneLight::Leave);
                    changed = true;
                }
            });
            if let Some(ZoneLight::Own(e)) = self.cfg.lighting.zones.get_mut(&z.id) {
                changed |= effect_editor(ui, e, Some(&z.kinds));
            }
            if let Some(err) = error {
                ui.label(RichText::new(err).font(theme::sans(12.0)).color(RED));
            }
        });
        changed
    }
}
