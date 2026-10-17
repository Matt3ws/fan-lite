//! egui front-end in lghub-lite's "Gunmetal" look. Repaints on input, when the engine
//! publishes a tick, and (only while fan icons spin and animations are on) at ~60 fps.
//! Nothing renders while minimized.

mod chrome;
mod curve_editor;
mod lighting;
mod rename;
mod theme;
mod tray;
mod updates;

use std::collections::{HashMap, VecDeque};
use std::f32::consts::TAU;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, Layout, RichText, Sense, UiBuilder, vec2};

use crate::config::{Config, Curve, FanSettings, Mode, Preset, SMOOTH_MAX};
use crate::control::Reason;
use crate::engine::{ControlInfo, EngineHandle, Snapshot};
use theme::{AMBER, DIM, DISPLAY, GREEN, Icon, LABEL, MINT, OUTLINE, RED, ROW_SEL, TEXT, display, pill};

const SAVE_DELAY: Duration = Duration::from_secs(1);
/// Frame interval while fan icons spin (~60 fps).
const ANIM_FRAME: Duration = Duration::from_millis(16);
const HISTORY: usize = 48;
/// Height of the big display and the gauge beside it.
const HERO_H: f32 = 130.0;

#[derive(PartialEq, Clone, Copy)]
enum View {
    Main,
    Curves,
    Lighting,
}

pub struct App {
    engine: EngineHandle,
    cfg: Config,
    messages: Vec<String>,
    dirty_since: Option<Instant>,
    view: View,
    sel_curve: usize,
    renaming: Option<rename::Rename>,
    /// Edit buffer for the selected curve's name.
    curve_name: Option<String>,
    /// Also list fans that are hidden (empty headers or hidden by the user).
    show_hidden: bool,
    /// Control id shown in the big display.
    selected: Option<String>,
    settings_open: bool,
    notices_open: bool,
    /// Icon rotation per control id, radians.
    angles: HashMap<String, f32>,
    /// Displayed rotation speed per control id (eases towards the real one).
    spin_rps: HashMap<String, f32>,
    /// Time of the last animated frame; `None` while nothing spins.
    anim_clock: Option<Instant>,
    /// Recent RPM (or duty when there is no tachometer) per control id, one sample per tick.
    history: HashMap<String, VecDeque<f32>>,
    last_tick: u64,
    /// Notification-area icon; `None` if the shell refused it (then close quits as before).
    tray: Option<tray::Tray>,
    /// Set by the tray menu's Quit so the next close request really closes.
    quitting: bool,
    /// Engine tick the tray tooltip was last built for.
    tip_tick: u64,
    /// Last calibration report the UI has handled (by sequence number).
    calib_seen: u64,
    /// Why the last calibration of a fan stopped early: (control id, message).
    calib_note: Option<(String, String)>,
    /// Showing the preset picker because Curve was chosen while no curves exist.
    picking_preset: bool,
    /// Control id of the fan row being dragged to a new position.
    dragging: Option<String>,
    /// Fans marked for mass editing (control ids, in click order). Empty = single selection.
    multi: Vec<String>,
    /// Lighting zone whose settings are open.
    light_sel: Option<String>,
    /// Outcome of the last "Save to device", shown under the zone list.
    light_note: Option<String>,
    light_save_seen: u64,
    update: updates::UpdateState,
    update_rx: Option<std::sync::mpsc::Receiver<updates::UpdateMsg>>,
    /// When GitHub was last asked for a new version.
    update_checked: Option<Instant>,
}

/// What a fan row/display needs, gathered once per frame.
struct FanRow<'a> {
    k: usize,
    info: &'a ControlInfo,
    visible: bool,
    rpm: Option<f32>,
    duty: Option<f32>,
}

fn mint(text: impl Into<String>, font: egui::FontId) -> RichText {
    RichText::new(text).font(font).color(MINT)
}

fn dim(text: impl Into<String>, font: egui::FontId) -> RichText {
    RichText::new(text).font(font).color(DIM)
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: Config, messages: Vec<String>) -> Self {
        theme::apply(&cc.egui_ctx);
        let hwnd = chrome::native_window_tweaks(cc);
        tray::set_ctx(cc.egui_ctx.clone());
        let tray = hwnd.and_then(tray::Tray::new);
        let ctx = cc.egui_ctx.clone();
        let engine = EngineHandle::spawn(cfg.clone(), Box::new(move || ctx.request_repaint()));
        App {
            engine,
            cfg,
            messages,
            dirty_since: None,
            view: View::Main,
            sel_curve: 0,
            renaming: None,
            curve_name: None,
            show_hidden: false,
            selected: None,
            settings_open: false,
            notices_open: false,
            angles: HashMap::new(),
            spin_rps: HashMap::new(),
            anim_clock: None,
            history: HashMap::new(),
            last_tick: 0,
            tray,
            quitting: false,
            tip_tick: 0,
            calib_seen: 0,
            calib_note: None,
            picking_preset: false,
            dragging: None,
            multi: Vec::new(),
            light_sel: None,
            light_note: None,
            light_save_seen: 0,
            update: updates::UpdateState::default(),
            update_rx: None,
            update_checked: None,
        }
    }

    /// Fans under our control are always shown. Otherwise an explicit hide/show wins, and
    /// motherboard headers that haven't reported any RPM since startup count as empty.
    fn control_visible(&self, info: &ControlInfo, snap: &Snapshot) -> bool {
        if self.cfg.fan(&info.id).is_some_and(|f| f.mode != Mode::Auto) {
            return true;
        }
        if let Some(&shown) = self.cfg.visibility.get(&info.id) {
            return shown;
        }
        !info.optional
            || info
                .rpm_fan
                .is_none_or(|f| snap.fan_seen.get(f).copied().unwrap_or(false))
    }

    fn save(&mut self) {
        self.dirty_since = None;
        if let Err(e) = self.cfg.save() {
            self.messages.push(format!("saving settings failed: {e}"));
        }
    }

    fn name_label(&mut self, ui: &mut egui::Ui, id: &str, default: &str) -> bool {
        rename::name_label(ui, &mut self.renaming, &mut self.cfg.labels, id, default)
    }

    /// Tray hover text: one line per visible fan with its speed, then the CPU temperature.
    fn tray_tip(&self, snap: &Snapshot) -> String {
        let mut tip = String::from("fan-lite");
        if snap.emergency {
            tip += "\nEMERGENCY: fans at 100%";
        }
        for (k, info) in snap.inv.controls.iter().enumerate() {
            if !self.control_visible(info, snap) {
                continue;
            }
            let rpm = info.rpm_fan.and_then(|f| snap.fans.get(f).copied().flatten());
            let duty = snap.controls.get(k).and_then(|c| c.duty);
            let value = match (rpm, duty) {
                (Some(r), _) => format!("{r:.0} rpm"),
                (None, Some(d)) => format!("{d:.0}%"),
                _ => continue,
            };
            tip += &format!("\n{} {value}", self.cfg.label(&info.id, &info.label));
        }
        let cpu = snap
            .inv
            .temps
            .iter()
            .enumerate()
            .find(|(_, t)| t.id.starts_with("amd/"))
            .or(snap.inv.temps.iter().enumerate().next());
        if let Some((i, t)) = cpu
            && let Some(v) = snap.temps.get(i).copied().flatten()
        {
            tip += &format!("\n{} {v:.0}°C", self.cfg.label(&t.id, &t.label));
        }
        tip
    }

    fn fan_settings(&self, id: &str) -> FanSettings {
        self.cfg
            .fan(id)
            .cloned()
            .unwrap_or_else(|| FanSettings::new(id.to_owned()))
    }

    fn angle(&self, id: &str) -> f32 {
        self.angles.get(id).copied().unwrap_or(0.0)
    }

    /// Advance icon rotation and record history. Returns true if any icon is spinning.
    fn animate(&mut self, snap: &Snapshot, rows: &[FanRow]) -> bool {
        if snap.tick != self.last_tick {
            self.last_tick = snap.tick;
            for r in rows {
                let h = self.history.entry(r.info.id.clone()).or_default();
                if h.len() == HISTORY {
                    h.pop_front();
                }
                h.push_back(r.rpm.or(r.duty).unwrap_or(0.0));
            }
        }
        if !self.cfg.animations {
            self.anim_clock = None;
            return false;
        }
        // Measured wall-clock time since the last animated frame. (egui's `stable_dt` is a
        // prediction after an idle wait, so with timed repaints it made the blades step by
        // the wrong amount and stutter.)
        let now = Instant::now();
        let dt = self
            .anim_clock
            .map_or(0.0, |t| now.duration_since(t).as_secs_f32())
            .min(0.1);
        let mut spinning = false;
        for r in rows.iter().filter(|r| r.visible || self.show_hidden) {
            let target = theme::visual_rps(r.rpm, r.duty);
            // Ease towards the new speed instead of jumping when RPM updates each tick.
            let rps = self.spin_rps.entry(r.info.id.clone()).or_insert(target);
            *rps += (target - *rps) * (dt * 4.0).min(1.0);
            if *rps > 0.01 {
                spinning = true;
                let a = self.angles.entry(r.info.id.clone()).or_insert(0.0);
                *a = (*a + *rps * TAU * dt) % TAU;
            }
        }
        self.anim_clock = spinning.then_some(now);
        spinning
    }

    // ---------------------------------------------------------------- title bar & banners

    fn light(&self, snap: &Snapshot) -> Color32 {
        if !self.engine.alive() || snap.emergency {
            RED
        } else {
            AMBER
        }
    }

    /// Extra title-bar items after "fan-lite": engine state and the notices toggle.
    fn title_extra(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        if !self.engine.alive() {
            ui.label(RichText::new("engine stopped").font(theme::sans(12.0)).color(RED));
        } else if !snap.ready {
            ui.label(RichText::new("detecting…").font(theme::sans(12.0)).color(LABEL));
        }
        let n = snap.inv.diagnostics.len() + self.messages.len();
        if n > 0 {
            let text = format!("{n} notice{}", if n == 1 { "" } else { "s" });
            let r = ui
                .add(egui::Label::new(RichText::new(text).font(theme::sans(12.0)).color(LABEL)).sense(Sense::click()));
            let icon = if self.notices_open { Icon::Up } else { Icon::Down };
            let (ir, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
            theme::paint_icon(ui.painter(), ir.center(), icon, LABEL);
            if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                self.notices_open = !self.notices_open;
            }
        }
        self.update_title(ui);
    }

    fn banners(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        if self.notices_open {
            display(ui, |ui| {
                for d in snap.inv.diagnostics.iter().chain(&self.messages) {
                    ui.label(mint(d, theme::mono(12.0)));
                }
            });
        }
        if snap.emergency {
            egui::Frame::new()
                .fill(RED)
                .stroke((1.0, OUTLINE))
                .corner_radius(10)
                .inner_margin(10)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new(format!(
                            "EMERGENCY · a watched sensor reached {:.0}°C",
                            self.cfg.critical_c
                        ))
                        .font(theme::mono_semi(13.0))
                        .color(OUTLINE),
                    );
                    ui.label(
                        RichText::new("all controlled fans forced to 100%")
                            .font(theme::mono(12.0))
                            .color(OUTLINE),
                    );
                });
        }
    }

    // ---------------------------------------------------------------- main view

    fn hero(&mut self, ui: &mut egui::Ui, snap: &Snapshot, row: &FanRow) {
        let view = &snap.controls[row.k];
        let fan = self.fan_settings(&row.info.id);
        let controlled = fan.mode != Mode::Auto;
        let name = self.cfg.label(&row.info.id, &row.info.label).to_owned();
        let alert = matches!(view.reason, Reason::Emergency | Reason::SensorFault | Reason::Fault);
        let accent = if alert { RED } else { AMBER };
        let angle = self.angle(&row.info.id);
        let meter_w = 30.0;
        // Fixed rects: display on the left, gauge on the right, 8 pt apart.
        let row_rect = egui::Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), HERO_H));
        ui.allocate_rect(row_rect, Sense::hover());
        let meter_rect = egui::Rect::from_min_max(egui::pos2(row_rect.right() - meter_w, row_rect.top()), row_rect.max);
        let disp_rect = egui::Rect::from_min_max(row_rect.min, egui::pos2(meter_rect.left() - 8.0, row_rect.bottom()));
        {
            let mut dui = ui.new_child(
                UiBuilder::new()
                    .max_rect(disp_rect)
                    .layout(Layout::top_down(egui::Align::Min)),
            );
            {
                theme::display_frame(12.0).show(&mut dui, |ui| {
                    // Frame adds 12 pt margins and a 1 pt stroke on each side.
                    ui.set_width(disp_rect.width() - 26.0);
                    ui.set_height(HERO_H - 26.0);
                    ui.spacing_mut().item_spacing.y = 6.0;
                    // Rows hug their text; the default 26 pt minimum row height would overflow.
                    ui.spacing_mut().interact_size.y = 16.0;
                    ui.horizontal(|ui| {
                        ui.add(egui::Label::new(mint(name.to_uppercase(), theme::mono(12.0))).truncate());
                        ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(dim(row.info.backend.to_uppercase(), theme::mono(12.0)));
                        });
                    });
                    ui.horizontal(|ui| {
                        theme::fan_widget(ui, 46.0, angle, accent);
                        let big = row.duty.map_or("--%".into(), |d| format!("{d:.0}%"));
                        let font = theme::grotesk(66.0);
                        let size = ui.painter().layout_no_wrap(big.clone(), font.clone(), accent).size();
                        let (r, _) = ui.allocate_exact_size(vec2(size.x + 4.0, 54.0), Sense::hover());
                        theme::glow_text(ui.painter(), r.left_center(), Align2::LEFT_CENTER, &big, font, accent);
                        ui.with_layout(Layout::top_down(egui::Align::Max), |ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.add_space(3.0);
                            ui.label(mint(
                                row.rpm.map_or("— rpm".into(), |r| format!("{r:.0} rpm")),
                                theme::mono_semi(13.0),
                            ));
                            let mode = match &fan.mode {
                                Mode::Auto => "auto · firmware".to_owned(),
                                Mode::Manual(p) => format!("manual · {p:.0}%"),
                                Mode::Curve(n) => format!("curve · {n}"),
                            };
                            ui.label(dim(mode, theme::mono(13.0)));
                            let status = if view.stalled { "stalled?" } else { view.reason.label() };
                            let col = if alert || view.stalled { RED } else { DIM };
                            let r = ui.label(RichText::new(status).font(theme::mono(13.0)).color(col));
                            if let Some(e) = &view.error {
                                r.on_hover_text(e);
                            }
                        });
                    });
                    // Chips sit on the bottom edge of the display, as in lghub-lite.
                    ui.with_layout(Layout::left_to_right(egui::Align::Max), |ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        theme::chip(ui, row.info.backend);
                        theme::chip(ui, &row.info.id);
                        if let Some(c) = view.commanded {
                            theme::chip(ui, &format!("set {c:.0}%"));
                        }
                    });
                });
            }
        }
        let floor = controlled.then(|| fan.min_pct.max(row.info.hw_min_pct) / 100.0);
        let mut mui = ui.new_child(UiBuilder::new().max_rect(meter_rect));
        theme::seg_meter(&mut mui, meter_rect.size(), row.duty.unwrap_or(0.0) / 100.0, floor);
    }

    fn history_strip(&self, ui: &mut egui::Ui, id: &str) {
        theme::display_frame(8.0).show(ui, |ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 23.0), Sense::hover());
            let p = ui.painter();
            let base = rect.bottom() - 1.0;
            let mut x = rect.left();
            while x < rect.right() {
                p.line_segment(
                    [egui::pos2(x, base), egui::pos2((x + 3.0).min(rect.right()), base)],
                    (1.0, DIM),
                );
                x += 6.0;
            }
            let Some(h) = self.history.get(id) else { return };
            let max = h.iter().copied().fold(1.0f32, f32::max);
            let (w, gap) = (11.0, 2.0);
            for (i, v) in h.iter().rev().enumerate() {
                let right = rect.right() - i as f32 * (w + gap);
                if right - w < rect.left() {
                    break;
                }
                if *v <= 0.0 {
                    continue;
                }
                let bh = (v / max * (rect.height() - 2.0)).max(7.0);
                let bar = egui::Rect::from_min_max(egui::pos2(right - w, base - bh), egui::pos2(right, base));
                p.rect_filled(bar.expand(1.0), 3, AMBER.gamma_multiply(0.12));
                p.rect_filled(bar, 2, AMBER);
            }
        });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, rows: &[FanRow], sel: usize) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let ids: Vec<&String> = rows
                .iter()
                .filter(|r| r.visible || self.show_hidden)
                .map(|r| &r.info.id)
                .collect();
            let pos = ids.iter().position(|id| **id == rows[sel].info.id).unwrap_or(0);
            if theme::button(ui, Some(Icon::Left), "", None, false)
                .on_hover_text("Previous fan")
                .clicked()
                && !ids.is_empty()
            {
                self.selected = Some(ids[(pos + ids.len() - 1) % ids.len()].clone());
                self.multi.clear();
            }
            if theme::button(ui, Some(Icon::Right), "", None, false)
                .on_hover_text("Next fan")
                .clicked()
                && !ids.is_empty()
            {
                self.selected = Some(ids[(pos + 1) % ids.len()].clone());
                self.multi.clear();
            }
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                let trail = if self.settings_open { Icon::Up } else { Icon::Down };
                if theme::button(ui, Some(Icon::Sliders), "Settings", Some(trail), self.settings_open).clicked() {
                    self.settings_open = !self.settings_open;
                }
                // The synced colour (or amber for multi-colour effects), dark when sync is off.
                let light_dot = match &self.cfg.lighting.sync {
                    Some(e) => e.swatch().map_or(AMBER, |c| Color32::from_rgb(c.0, c.1, c.2)),
                    None => OUTLINE,
                };
                if theme::button(ui, Some(Icon::Dot(light_dot)), "Light", None, false)
                    .on_hover_text("RGB lighting for every device")
                    .clicked()
                {
                    self.view = View::Lighting;
                }
                let dot = if self.cfg.animations { AMBER } else { OUTLINE };
                if theme::button(ui, Some(Icon::Dot(dot)), "Spin", None, false)
                    .on_hover_text("Animate fan icons (redraws ~60×/s while visible)")
                    .clicked()
                {
                    self.cfg.animations = !self.cfg.animations;
                    changed = true;
                }
                // Recessed − value + box, like lghub-lite's "Alert at".
                theme::display_frame(0.0)
                    .corner_radius(7)
                    .inner_margin(egui::Margin::symmetric(4, 0))
                    .show(ui, |ui| {
                        // Fixed size, explicitly left-to-right (a plain `horizontal` would inherit
                        // right-to-left here, and `with_layout` would stretch to the free space).
                        ui.allocate_ui_with_layout(
                            vec2(68.0, 25.0),
                            Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.spacing_mut().item_spacing.x = 2.0;
                                if theme::icon_hit(ui, Icon::Minus, MINT).clicked() {
                                    self.cfg.critical_c -= 1.0;
                                    changed = true;
                                }
                                ui.label(
                                    RichText::new(format!("{:.0}°", self.cfg.critical_c))
                                        .font(theme::mono(13.0))
                                        .color(RED),
                                )
                                .on_hover_text("If any watched sensor reaches this, every controlled fan goes to 100%");
                                if theme::icon_hit(ui, Icon::Plus, MINT).clicked() {
                                    self.cfg.critical_c += 1.0;
                                    changed = true;
                                }
                            },
                        );
                    });
                // Only if it fits beside the ◀ ▶ buttons (narrow windows drop the label).
                let label_w = ui
                    .painter()
                    .layout_no_wrap("Alert at".into(), theme::sans_med(12.0), LABEL)
                    .size()
                    .x;
                if ui.available_width() >= label_w + 2.0 {
                    ui.label(RichText::new("Alert at").font(theme::sans_med(12.0)).color(LABEL))
                        .on_hover_text(
                            "Critical temperature: if any watched sensor reaches it, every controlled fan goes to 100%",
                        );
                }
            });
        });
        changed
    }

    /// Sensor presets are built on: an AIO's liquid temperature if there is one (steadier and
    /// what the radiator actually sees), else the CPU's Tctl, else the first temperature.
    fn preset_sensor(snap: &Snapshot) -> String {
        snap.inv
            .temps
            .iter()
            .find(|t| t.id == crate::config::LIQUID_SENSOR)
            .or_else(|| snap.inv.temps.iter().find(|t| t.id == "amd/tctl"))
            .or(snap.inv.temps.first())
            .map_or(String::new(), |t| t.id.clone())
    }

    /// Add a curve from `preset` and return its name. With `reuse`, an existing curve of the
    /// preset's name is used instead of adding a copy.
    fn add_preset(&mut self, preset: Preset, snap: &Snapshot, reuse: bool) -> String {
        if reuse && self.cfg.curve(preset.name()).is_some() {
            return preset.name().to_owned();
        }
        let mut name = preset.name().to_owned();
        let mut n = 2;
        while self.cfg.curve(&name).is_some() {
            name = format!("{} {n}", preset.name());
            n += 1;
        }
        self.cfg
            .curves
            .push(Curve::preset(preset, name.clone(), Self::preset_sensor(snap)));
        name
    }

    /// "Test" row: Calibrate button, progress, last result and what it means.
    fn calibration_rows(&mut self, ui: &mut egui::Ui, snap: &Snapshot, row: &FanRow, f: &mut FanSettings) -> bool {
        let mut changed = false;
        let id = &row.info.id;
        let key = |ui: &mut egui::Ui, t: &str| {
            ui.add_sized(vec2(62.0, 26.0), egui::Label::new(dim(t, theme::sans(13.0))));
        };
        ui.horizontal(|ui| {
            key(ui, "Test");
            match &snap.calibrating {
                Some((cid, duty, step, of)) if cid == id => {
                    ui.spinner();
                    ui.label(mint(format!("Calibrating {duty:.0}% · {step}/{of}"), theme::sans(13.0)));
                }
                Some(_) => {
                    ui.label(dim("another fan is being calibrated", theme::sans(13.0)));
                }
                None => {
                    if row.info.rpm_fan.is_none() {
                        ui.label(dim("no speed sensor to measure", theme::sans(13.0)));
                    } else if pill(ui, false, "Calibrate")
                        .on_hover_text(
                            "Steps this output from 100% down to 20% (never below its minimum) for about \
                             45 s and measures the speed at each step. Stops if the CPU gets hot.",
                        )
                        .clicked()
                    {
                        self.engine.calibrate(id);
                        self.calib_note = None;
                    }
                }
            }
        });
        if let Some((nid, msg)) = &self.calib_note
            && nid == id
        {
            ui.label(RichText::new(msg).font(theme::sans(12.0)).color(AMBER));
        }
        let Some(points) = self.cfg.calibrations.get(id) else {
            return changed;
        };
        let Some(v) = crate::control::analyze(points) else {
            return changed;
        };
        let (lo, hi) = (
            points
                .iter()
                .copied()
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .unwrap_or_default(),
            points
                .iter()
                .copied()
                .max_by(|a, b| a.0.total_cmp(&b.0))
                .unwrap_or_default(),
        );
        ui.label(mint(
            format!("{:.0}% → {:.0} rpm   ·   {:.0}% → {:.0} rpm", lo.0, lo.1, hi.0, hi.1),
            theme::mono(12.0),
        ));
        let (msg, color) = if !v.responds {
            (
                "Doesn't respond to PWM: curves won't change its speed. Normal for some pumps; for a \
                 fan, check the header's DC/PWM mode in the BIOS.",
                AMBER,
            )
        } else if v.likely_pump {
            (
                "Looks like a pump: still fast at its lowest duty. Use the AIO pump preset.",
                MINT,
            )
        } else {
            ("Responds to PWM, so curves will control it.", MINT)
        };
        ui.label(RichText::new(msg).font(theme::sans(12.0)).color(color));
        if v.responds && v.likely_pump && pill(ui, false, "Use AIO pump preset").clicked() {
            let name = self.add_preset(Preset::AioPump, snap, true);
            f.mode = Mode::Curve(name);
            changed = true;
        }
        changed
    }

    fn settings_drawer(&mut self, ui: &mut egui::Ui, snap: &Snapshot, row: &FanRow) -> bool {
        let id = row.info.id.clone();
        let before = self.fan_settings(&id);
        let mut f = before.clone();
        let mut changed = false;
        let key = |ui: &mut egui::Ui, t: &str| {
            ui.add_sized(vec2(62.0, 26.0), egui::Label::new(dim(t, theme::sans(13.0))));
        };
        // Mass edit: the drawer shows this fan's values, and changed fields go to every marked fan.
        let mass = self.multi.len() > 1;
        theme::display_frame(12.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = vec2(6.0, 8.0);
            if mass {
                let names: Vec<String> = self
                    .multi
                    .iter()
                    .map(|m| {
                        let default = snap
                            .inv
                            .controls
                            .iter()
                            .find(|c| c.id == *m)
                            .map_or(m.as_str(), |c| c.label.as_str());
                        self.cfg.label(m, default).to_owned()
                    })
                    .collect();
                ui.horizontal(|ui| {
                    key(ui, "Selected");
                    // Clear first (right-aligned), so the name list truncates in the space left.
                    ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                        if pill(ui, false, "Clear")
                            .on_hover_text("Edit one fan again (Esc)")
                            .clicked()
                        {
                            self.multi.clear();
                        }
                        ui.with_layout(Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.add(
                                egui::Label::new(mint(
                                    format!("{} fans: {}", names.len(), names.join(", ")),
                                    theme::sans(13.0),
                                ))
                                .truncate(),
                            );
                        });
                    });
                });
            } else {
                ui.horizontal(|ui| {
                    key(ui, "Name");
                    changed |= self.name_label(ui, &id, &row.info.label);
                    ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(dim("click to rename", theme::sans(12.0)));
                    });
                });
            }
            ui.horizontal(|ui| {
                key(ui, "Mode");
                if pill(ui, f.mode == Mode::Auto, "Auto")
                    .on_hover_text("Firmware/driver controls this fan")
                    .clicked()
                {
                    f.mode = Mode::Auto;
                }
                if pill(ui, matches!(f.mode, Mode::Manual(_)), "Manual").clicked() && !matches!(f.mode, Mode::Manual(_))
                {
                    f.mode = Mode::Manual(row.duty.unwrap_or(50.0).max(f.min_pct).round());
                }
                let curve_sel = matches!(f.mode, Mode::Curve(_)) || self.picking_preset;
                if pill(ui, curve_sel, "Curve").clicked() && !matches!(f.mode, Mode::Curve(_)) {
                    if let Some(c) = self.cfg.curves.first() {
                        f.mode = Mode::Curve(c.name.clone());
                    } else {
                        // No curves yet: offer the presets instead of an empty mode.
                        self.picking_preset = true;
                    }
                }
            });
            if self.picking_preset && !matches!(f.mode, Mode::Curve(_)) {
                ui.horizontal(|ui| {
                    key(ui, "Preset");
                    // Wrapped pills stay in their own column, under each other.
                    ui.horizontal_wrapped(|ui| {
                        for p in Preset::ALL {
                            if pill(ui, false, p.name()).clicked() {
                                let name = self.add_preset(p, snap, true);
                                f.mode = Mode::Curve(name);
                                self.picking_preset = false;
                            }
                        }
                    });
                });
            } else {
                self.picking_preset = false;
            }
            let floor = f.min_pct.max(row.info.hw_min_pct);
            if let Mode::Manual(p) = &mut f.mode {
                ui.horizontal(|ui| {
                    key(ui, "Speed");
                    ui.add(egui::Slider::new(p, floor..=100.0).suffix("%").integer());
                });
            }
            if let Mode::Curve(name) = &mut f.mode {
                ui.horizontal(|ui| {
                    key(ui, "Curve");
                    ui.horizontal_wrapped(|ui| {
                        for c in &self.cfg.curves {
                            if pill(ui, *name == c.name, &c.name).clicked() {
                                *name = c.name.clone();
                            }
                        }
                        if pill(ui, false, "Edit…").clicked() {
                            self.view = View::Curves;
                        }
                    });
                });
            }
            if f.mode != Mode::Auto {
                ui.horizontal(|ui| {
                    key(ui, "Min");
                    for v in [0.0, 20.0, 30.0, 40.0] {
                        if pill(ui, f.min_pct == v, &format!("{v:.0}%")).clicked() {
                            f.min_pct = v;
                        }
                    }
                    ui.add(egui::DragValue::new(&mut f.min_pct).range(0.0..=100.0).suffix("%"))
                        .on_hover_text("Lowest duty ever commanded. 0% lets the fan stop.");
                    if row.info.hw_min_pct > 0.0 {
                        ui.label(dim(format!("hw {:.0}%", row.info.hw_min_pct), theme::sans(12.0)));
                    }
                });
                ui.horizontal(|ui| {
                    key(ui, "Ramp");
                    ui.label(mint("↑", theme::sans(13.0)));
                    ui.add(egui::DragValue::new(&mut f.ramp_up).range(1.0..=100.0).suffix("%/s"));
                    ui.label(mint("↓", theme::sans(13.0)));
                    ui.add(egui::DragValue::new(&mut f.ramp_down).range(1.0..=100.0).suffix("%/s"));
                });
            }
            if !mass {
                changed |= self.calibration_rows(ui, snap, row, &mut f);
            }
            ui.horizontal(|ui| {
                key(ui, "Interval");
                for ms in [500u32, 1000, 2000, 5000] {
                    let label = if ms < 1000 {
                        format!("{ms}ms")
                    } else {
                        format!("{}s", ms / 1000)
                    };
                    if pill(ui, self.cfg.interval_ms == ms, &label).clicked() {
                        self.cfg.interval_ms = ms;
                        changed = true;
                    }
                }
                if f.mode == Mode::Auto && !mass {
                    ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                        let (txt, show) = if row.visible {
                            ("Hide fan", false)
                        } else {
                            ("Show fan", true)
                        };
                        if pill(ui, false, txt).clicked() {
                            self.cfg.visibility.insert(id.clone(), show);
                            changed = true;
                        }
                    });
                }
            });
            changed |= self.version_row(ui, key);
        });
        if f != before {
            if mass {
                // Only the fields that were changed are copied; each fan keeps the rest.
                for m in self.multi.clone() {
                    let target = self.fan_settings(&m);
                    *self.cfg.fan_mut(&m) = apply_changes(&before, &f, &target);
                }
            } else {
                *self.cfg.fan_mut(&id) = f;
            }
            changed = true;
        }
        changed
    }

    /// List panel, as lghub-lite's device list: one recessed panel, rows inside it.
    fn list_panel<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
        theme::display_frame(4.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.spacing_mut().interact_size.y = 18.0;
                add(ui)
            })
            .inner
    }

    /// One 32 pt list row; the selected row gets a slightly lighter, rounded background.
    fn row_frame(selected: bool) -> egui::Frame {
        egui::Frame::new()
            .fill(if selected { ROW_SEL } else { Color32::TRANSPARENT })
            .corner_radius(7)
            .inner_margin(egui::Margin::symmetric(10, 7))
    }

    /// Right-aligned value with a level bar in front of it.
    fn row_value(ui: &mut egui::Ui, value: String, frac: f32, color: Color32) {
        ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_sized(
                vec2(64.0, 18.0),
                egui::Label::new(RichText::new(value).font(theme::mono_semi(12.0)).color(color)),
            );
            ui.add_space(4.0);
            theme::hbar(ui, 45.0, frac, color);
        });
    }

    /// The fan list. Rows can be dragged to reorder them; returns true if the order changed.
    fn fan_list(&mut self, ui: &mut egui::Ui, snap: &Snapshot, rows: &[FanRow], sel: usize) -> bool {
        let mut changed = false;
        theme::section(ui, "FANS");
        Self::list_panel(ui, |ui| {
            // (index into `rows`, on-screen rect) of each shown row, for drag-to-reorder.
            let mut shown: Vec<(usize, egui::Rect)> = Vec::new();
            let mut n = 0;
            for (i, r) in rows.iter().enumerate() {
                if !r.visible && !self.show_hidden {
                    continue;
                }
                n += 1;
                let view = &snap.controls[r.k];
                let alert = matches!(view.reason, Reason::Emergency | Reason::SensorFault | Reason::Fault);
                let color = if alert {
                    RED
                } else if i == sel {
                    AMBER
                } else {
                    GREEN
                };
                let name = self.cfg.label(&r.info.id, &r.info.label).to_owned();
                let angle = self.angle(&r.info.id);
                let dragged = self.dragging.as_deref() == Some(r.info.id.as_str());
                let marked = self.multi.len() > 1 && self.multi.contains(&r.info.id);
                let resp = ui
                    .scope_builder(UiBuilder::new().sense(Sense::click_and_drag()), |ui| {
                        Self::row_frame(i == sel || marked).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            if !r.visible || dragged {
                                ui.multiply_opacity(if dragged { 0.35 } else { 0.5 });
                            }
                            ui.horizontal(|ui| {
                                ui.label(dim(n.to_string(), theme::mono(12.0)));
                                theme::fan_widget(ui, 16.0, angle, color);
                                ui.add(
                                    egui::Label::new(mint(&name, theme::sans(13.0)))
                                        .truncate()
                                        .selectable(false),
                                );
                                let value = match (r.rpm, r.duty) {
                                    (Some(rpm), _) => format!("{rpm:.0} rpm"),
                                    (None, Some(d)) => format!("{d:.0}%"),
                                    _ => "--".into(),
                                };
                                Self::row_value(ui, value, r.duty.unwrap_or(0.0) / 100.0, color);
                            });
                        });
                    })
                    .response;
                if resp.drag_started() {
                    self.dragging = Some(r.info.id.clone());
                }
                if marked {
                    // Amber edge on every row in a multi-selection.
                    let rr = resp.rect;
                    ui.painter().line_segment(
                        [
                            egui::pos2(rr.left() + 1.5, rr.top() + 6.0),
                            egui::pos2(rr.left() + 1.5, rr.bottom() - 6.0),
                        ],
                        egui::Stroke::new(2.0, AMBER),
                    );
                }
                if resp.clicked() {
                    let m = ui.input(|i| i.modifiers);
                    let id = r.info.id.clone();
                    if m.command || m.ctrl {
                        // Toggle this fan; the first Ctrl+click also takes the current one along.
                        if self.multi.is_empty() {
                            self.multi.push(rows[sel].info.id.clone());
                        }
                        match self.multi.iter().position(|x| *x == id) {
                            Some(p) if self.multi.len() > 1 => {
                                self.multi.remove(p);
                            }
                            Some(_) => {}
                            None => self.multi.push(id.clone()),
                        }
                        self.selected = Some(if self.multi.contains(&id) {
                            id
                        } else {
                            self.multi[self.multi.len() - 1].clone()
                        });
                    } else if m.shift {
                        // Range from the current fan to this one, in list order.
                        let (a, b) = (sel.min(i), sel.max(i));
                        self.multi = rows[a..=b]
                            .iter()
                            .filter(|x| x.visible || self.show_hidden)
                            .map(|x| x.info.id.clone())
                            .collect();
                        self.selected = Some(id);
                    } else {
                        self.multi.clear();
                        self.selected = Some(id);
                    }
                }
                shown.push((i, resp.rect));
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.multi.clear();
            }

            // While dragging: an amber line marks where the row will land; dropping applies it.
            let Some(drag_id) = self.dragging.clone() else { return };
            let Some(pos) = ui.ctx().pointer_interact_pos() else {
                return;
            };
            if shown.is_empty() {
                return;
            }
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            let slot = shown.iter().filter(|(_, r)| r.center().y < pos.y).count();
            let y = if slot == 0 {
                shown[0].1.top()
            } else {
                shown[slot - 1].1.bottom()
            };
            let (x0, x1) = (shown[0].1.left() + 6.0, shown[0].1.right() - 6.0);
            ui.painter()
                .line_segment([egui::pos2(x0, y), egui::pos2(x1, y)], egui::Stroke::new(2.0, AMBER));
            if ui.input(|i| i.pointer.any_released()) {
                let mut ids: Vec<String> = shown.iter().map(|(i, _)| rows[*i].info.id.clone()).collect();
                if let Some(from) = ids.iter().position(|x| *x == drag_id) {
                    let item = ids.remove(from);
                    let to = if slot > from { slot - 1 } else { slot };
                    ids.insert(to.min(ids.len()), item);
                }
                // Keep the saved position of fans that aren't shown right now (e.g. hidden ones).
                for old in &self.cfg.fan_order {
                    if !ids.contains(old) {
                        ids.push(old.clone());
                    }
                }
                if ids != self.cfg.fan_order {
                    self.cfg.fan_order = ids;
                    changed = true;
                }
                self.dragging = None;
            }
        });
        if ui.input(|i| i.pointer.any_released()) {
            self.dragging = None;
        }
        let hidden = rows.iter().filter(|r| !r.visible).count();
        ui.horizontal(|ui| {
            if hidden > 0 {
                let text = if self.show_hidden {
                    "hide empty headers".to_owned()
                } else {
                    format!("show {hidden} hidden fan{}", if hidden == 1 { "" } else { "s" })
                };
                let r = ui.add(
                    egui::Label::new(RichText::new(text).font(theme::sans(12.0)).color(LABEL)).sense(Sense::click()),
                );
                if r.on_hover_text("Motherboard headers with no RPM since startup are hidden automatically")
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    self.show_hidden = !self.show_hidden;
                }
            }
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(dim("Ctrl/Shift+click to select several", theme::sans(12.0)));
            });
        });
        changed
    }

    fn temp_list(&mut self, ui: &mut egui::Ui, snap: &Snapshot) -> bool {
        let mut changed = false;
        theme::section(ui, "TEMPERATURES");
        Self::list_panel(ui, |ui| {
            for (i, t) in snap.inv.temps.iter().enumerate() {
                let v = snap.temps.get(i).copied().flatten();
                let frac = v.map_or(0.0, |v| v / self.cfg.critical_c);
                let color = match frac {
                    f if f >= 1.0 => RED,
                    f if f >= 0.8 => AMBER,
                    _ => GREEN,
                };
                let watched = !self.cfg.unwatched.contains(&t.id);
                Self::row_frame(false).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let tip = if watched {
                            "Watched: can trigger the emergency override. Click to ignore."
                        } else {
                            "Ignored by the emergency override. Click to watch."
                        };
                        let (r, resp) = ui.allocate_exact_size(vec2(10.0, 18.0), Sense::click());
                        let fill = if watched { GREEN } else { DISPLAY };
                        ui.painter()
                            .circle(r.center(), 3.0, fill, (1.0, if watched { GREEN } else { DIM }));
                        if resp
                            .on_hover_text(tip)
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .clicked()
                        {
                            if watched {
                                self.cfg.unwatched.insert(t.id.clone());
                            } else {
                                self.cfg.unwatched.remove(&t.id);
                            }
                            changed = true;
                        }
                        changed |= self.name_label(ui, &t.id, &t.label);
                        Self::row_value(ui, v.map_or("--".into(), |v| format!("{v:.1}°C")), frac, color);
                    });
                });
            }
        });
        changed
    }

    fn main_view(&mut self, ui: &mut egui::Ui, snap: &Snapshot, rows: &[FanRow]) -> bool {
        let mut changed = false;
        let shown: Vec<usize> = (0..rows.len())
            .filter(|&i| rows[i].visible || self.show_hidden)
            .collect();
        let sel = self
            .selected
            .as_ref()
            .and_then(|id| shown.iter().copied().find(|&i| rows[i].info.id == *id))
            .or(shown.first().copied());
        match sel {
            Some(sel) => {
                self.hero(ui, snap, &rows[sel]);
                self.history_strip(ui, &rows[sel].info.id);
                changed |= self.toolbar(ui, rows, sel);
                if self.settings_open {
                    changed |= self.settings_drawer(ui, snap, &rows[sel]);
                }
                changed |= self.fan_list(ui, snap, rows, sel);
            }
            None => {
                display(ui, |ui| {
                    let msg = if snap.ready {
                        "NO FANS DETECTED · SEE NOTICES"
                    } else {
                        "DETECTING HARDWARE…"
                    };
                    ui.label(mint(msg, theme::mono(13.0)));
                });
            }
        }
        changed |= self.temp_list(ui, snap);
        changed
    }

    // ---------------------------------------------------------------- curves view

    fn curves_view(&mut self, ui: &mut egui::Ui, snap: &Snapshot) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            if theme::button(ui, Some(Icon::Left), "Back", None, false).clicked() {
                self.view = View::Main;
            }
            ui.label(RichText::new("Curves").font(theme::sans_semi(14.0)).color(TEXT));
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::button(ui, Some(Icon::Plus), "New", None, false).clicked() {
                    let mut n = self.cfg.curves.len() + 1;
                    while self.cfg.curve(&format!("Curve {n}")).is_some() {
                        n += 1;
                    }
                    let sensor = snap.inv.temps.first().map_or(String::new(), |t| t.id.clone());
                    self.cfg.curves.push(Curve::new(format!("Curve {n}"), sensor));
                    self.sel_curve = self.cfg.curves.len() - 1;
                    self.curve_name = None;
                    changed = true;
                }
            });
        });
        // Presets tuned for a Ryzen X3D's Tctl; each click adds a new, editable curve.
        theme::display_frame(10.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(dim("Presets", theme::sans(13.0)));
                ui.horizontal_wrapped(|ui| {
                    for p in Preset::ALL {
                        if pill(ui, false, &format!("+ {}", p.name())).clicked() {
                            let name = self.add_preset(p, snap, false);
                            self.sel_curve = self.cfg.curves.iter().position(|c| c.name == name).unwrap_or(0);
                            self.curve_name = None;
                            changed = true;
                        }
                    }
                });
            });
        });
        let Some(curve) = self.cfg.curves.get(self.sel_curve).cloned() else {
            display(ui, |ui| {
                ui.label(mint("A curve maps one temperature to a fan speed.", theme::sans(13.0)));
                ui.label(dim(
                    "Add a preset above, then pick it as a fan's mode.",
                    theme::sans(13.0),
                ));
            });
            return changed;
        };
        let users: Vec<String> = self
            .cfg
            .fans
            .iter()
            .filter(|f| f.mode == Mode::Curve(curve.name.clone()))
            .map(|f| f.id.clone())
            .collect();

        let mut c = curve.clone();
        let mut deleted = false;
        theme::display_frame(12.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = vec2(6.0, 8.0);
            ui.horizontal_wrapped(|ui| {
                for (i, cv) in self.cfg.curves.iter().enumerate() {
                    if pill(ui, self.sel_curve == i, &cv.name).clicked() {
                        self.sel_curve = i;
                        self.curve_name = None;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.add_sized(vec2(56.0, 26.0), egui::Label::new(dim("Name", theme::sans(13.0))));
                let buf = self.curve_name.get_or_insert_with(|| c.name.clone());
                let r = ui.add(egui::TextEdit::singleline(buf).desired_width(130.0).text_color(MINT));
                if r.lost_focus() {
                    let new = buf.trim().to_owned();
                    if !new.is_empty() && new != c.name && self.cfg.curve(&new).is_none() {
                        for f in &mut self.cfg.fans {
                            if f.mode == Mode::Curve(c.name.clone()) {
                                f.mode = Mode::Curve(new.clone());
                            }
                        }
                        c.name = new;
                    }
                    self.curve_name = None;
                }
                ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                    let del = ui.add_enabled(users.is_empty(), egui::Button::new("Delete"));
                    if users.is_empty() {
                        if del.clicked() {
                            deleted = true;
                        }
                    } else {
                        del.on_disabled_hover_text(format!("In use by: {}", users.join(", ")));
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.add_sized(vec2(56.0, 26.0), egui::Label::new(dim("Sensor", theme::sans(13.0))));
                let sel = snap
                    .inv
                    .temps
                    .iter()
                    .find(|t| t.id == c.sensor)
                    .map_or(c.sensor.clone(), |t| self.cfg.label(&t.id, &t.label).to_owned());
                egui::ComboBox::from_id_salt("curve_sensor")
                    .selected_text(sel)
                    .width(ui.available_width() - 8.0)
                    .truncate()
                    .show_ui(ui, |ui| {
                        for t in &snap.inv.temps {
                            ui.selectable_value(&mut c.sensor, t.id.clone(), self.cfg.label(&t.id, &t.label));
                        }
                    });
            });
            // Own row: next to a long sensor name these would push the panel past the window.
            ui.horizontal(|ui| {
                ui.add_sized(vec2(56.0, 26.0), egui::Label::new(dim("Hyst", theme::sans(13.0))));
                ui.add(
                    egui::DragValue::new(&mut c.hysteresis)
                        .range(0.0..=15.0)
                        .speed(0.1)
                        .suffix("°C"),
                )
                .on_hover_text("The fan only slows down after the temperature falls this much");
                ui.label(dim("Smooth", theme::sans(13.0)));
                ui.add(
                    egui::DragValue::new(&mut c.smooth)
                        .range(0.0..=SMOOTH_MAX)
                        .speed(0.1)
                        .suffix(" s"),
                )
                .on_hover_text(
                    "Averages the temperature over about this many seconds before the curve, so short \
                         CPU spikes don't make the fans surge. The emergency override ignores this.",
                );
            });
        });
        if deleted {
            self.cfg.curves.remove(self.sel_curve);
            self.sel_curve = self.sel_curve.saturating_sub(1);
            self.curve_name = None;
            return true;
        }

        let live = snap
            .curve_temps
            .iter()
            .find(|(n, _)| *n == curve.name)
            .map(|&(_, t)| (t, c.eval(t)))
            .or_else(|| snap.temp(&c.sensor).map(|t| (t, c.eval(t))));
        theme::display_frame(12.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            curve_editor::show(ui, &mut c, live);
            ui.horizontal_wrapped(|ui| {
                let mut remove = None;
                for (i, p) in c.points.iter_mut().enumerate() {
                    ui.add(egui::DragValue::new(&mut p.0).range(0.0..=120.0).suffix("°"));
                    ui.add(egui::DragValue::new(&mut p.1).range(0.0..=100.0).suffix("%"));
                    if theme::icon_hit(ui, Icon::Close, RED)
                        .on_hover_text("Remove point")
                        .clicked()
                    {
                        remove = Some(i);
                    }
                    ui.add_space(4.0);
                }
                if let Some(i) = remove.filter(|_| c.points.len() > 2) {
                    c.points.remove(i);
                }
                if pill(ui, false, "+ point").clicked() {
                    let last = *c.points.last().unwrap();
                    c.points.push(((last.0 + 5.0).min(120.0), last.1));
                }
            });
        });
        c.normalize();
        if c != curve {
            self.cfg.curves[self.sel_curve] = c;
            changed = true;
        }
        changed
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Runs every frame and, while the window is hidden in the tray, once per engine tick.
        let events = tray::take_events();
        if events & tray::QUIT != 0 {
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if events & tray::SHOW != 0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        if events & tray::READD != 0
            && let Some(t) = &mut self.tray
        {
            t.add();
        }
        // Closing (× or Alt+F4) hides to the tray and fan control keeps running; only the
        // tray menu's Quit really exits. Without a tray icon, close quits as before.
        if self.tray.is_some() && !self.quitting && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        if self.tray.is_some() {
            let snap = self.engine.snapshot().clone();
            if snap.tick != self.tip_tick {
                self.tip_tick = snap.tick;
                let tip = self.tray_tip(&snap);
                if let Some(t) = &mut self.tray {
                    t.set_tip(&tip);
                }
            }
        }

        self.poll_update(ctx);

        // Debounced save; runs even while the window is minimized.
        if let Some(t) = self.dirty_since {
            let left = SAVE_DELAY.saturating_sub(t.elapsed());
            if left.is_zero() {
                self.save();
            } else {
                ctx.request_repaint_after(left);
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let snap = self.engine.snapshot().clone();
        let mut rows: Vec<FanRow> = snap
            .inv
            .controls
            .iter()
            .enumerate()
            .filter(|(k, _)| *k < snap.controls.len())
            .map(|(k, info)| FanRow {
                k,
                info,
                visible: self.control_visible(info, &snap),
                rpm: info.rpm_fan.and_then(|f| snap.fans.get(f).copied().flatten()),
                duty: snap.controls[k].duty,
            })
            .collect();
        // The user's drag order first; fans it doesn't mention keep detection order after them.
        rows.sort_by_key(|r| {
            self.cfg
                .fan_order
                .iter()
                .position(|id| *id == r.info.id)
                .unwrap_or(usize::MAX)
        });
        let spinning = self.animate(&snap, &rows);
        let light = self.light(&snap);

        let mut changed = false;
        // A calibration finished (or stopped) since last frame: keep the result.
        if let Some(r) = &snap.last_calib
            && r.seq != self.calib_seen
        {
            self.calib_seen = r.seq;
            match &r.aborted {
                Some(msg) => self.calib_note = Some((r.id.clone(), msg.clone())),
                None if !r.results.is_empty() => {
                    self.cfg.calibrations.insert(r.id.clone(), r.results.clone());
                    changed = true;
                }
                None => {}
            }
        }
        if let Some((seq, errors)) = &snap.light_save
            && *seq != self.light_save_seen
        {
            self.light_save_seen = *seq;
            self.light_note = Some(if errors.is_empty() {
                "Saved to the devices.".into()
            } else {
                format!("Saving failed: {}", errors.join("; "))
            });
        }
        let mut content_h = 0.0;
        // Both closures need `self`; the title runs first, then the content.
        let this = std::cell::RefCell::new(&mut *self);
        chrome::window(
            ui,
            light,
            |ui| this.borrow_mut().title_extra(ui, &snap),
            |ui| {
                let out = egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    let mut app = this.borrow_mut();
                    app.banners(ui, &snap);
                    changed |= match app.view {
                        View::Main => app.main_view(ui, &snap, &rows),
                        View::Curves => app.curves_view(ui, &snap),
                        View::Lighting => app.lighting_view(ui, &snap),
                    };
                });
                content_h = out.content_size.y;
            },
        );
        chrome::fit_height(ui.ctx(), content_h);
        if changed {
            self.cfg.normalize();
            self.engine.set_config(self.cfg.clone());
            self.dirty_since.get_or_insert_with(Instant::now);
            ui.ctx().request_repaint_after(SAVE_DELAY);
        }
        if spinning && self.view == View::Main {
            ui.ctx().request_repaint_after(ANIM_FRAME);
        }
    }

    /// Transparent, so the frame's rounded corners show the desktop behind them.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.dirty_since.is_some() {
            self.save();
        }
    }
}

/// Window and taskbar icon: the same amber fan as the tray icon.
pub fn window_icon() -> egui::IconData {
    const SIZE: u32 = 64;
    egui::IconData {
        rgba: tray::fan_icon_rgba(SIZE),
        width: SIZE,
        height: SIZE,
    }
}

/// Mass edit: copy only the fields that changed between `before` and `after` (the primary
/// fan's settings as shown in the drawer) onto another selected fan's settings.
fn apply_changes(before: &FanSettings, after: &FanSettings, target: &FanSettings) -> FanSettings {
    let mut t = target.clone();
    if after.mode != before.mode {
        t.mode = after.mode.clone();
    }
    if after.min_pct != before.min_pct {
        t.min_pct = after.min_pct;
    }
    if after.ramp_up != before.ramp_up {
        t.ramp_up = after.ramp_up;
    }
    if after.ramp_down != before.ramp_down {
        t.ramp_down = after.ramp_down;
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fan(id: &str, mode: Mode, min: f32) -> FanSettings {
        FanSettings {
            mode,
            min_pct: min,
            ..FanSettings::new(id.into())
        }
    }

    #[test]
    fn mass_edit_copies_only_what_changed() {
        let before = fan("a", Mode::Auto, 20.0);
        let other = FanSettings {
            ramp_down: 4.0,
            ..fan("b", Mode::Manual(55.0), 35.0)
        };

        // Only Min changed: the other fan keeps its own mode and ramps.
        let after = FanSettings {
            min_pct: 30.0,
            ..before.clone()
        };
        let t = apply_changes(&before, &after, &other);
        assert_eq!(
            (t.mode.clone(), t.min_pct, t.ramp_down, t.id.as_str()),
            (Mode::Manual(55.0), 30.0, 4.0, "b")
        );

        // Only the mode changed: copied, the other fan's own Min and ramps are kept.
        let after = FanSettings {
            mode: Mode::Curve("Quiet".into()),
            ..before.clone()
        };
        let t = apply_changes(&before, &after, &other);
        assert_eq!(t.mode, Mode::Curve("Quiet".into()));
        assert_eq!((t.min_pct, t.ramp_down), (35.0, 4.0));

        // Nothing changed: untouched.
        assert_eq!(apply_changes(&before, &before, &other), other);
    }
}
