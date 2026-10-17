//! In-app updates: check GitHub at start and once a day (if enabled), and install the newest
//! release with one click, then restart into it. The work runs on a short-lived thread.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText, Sense};

use super::theme::{self, AMBER, LABEL, RED, pill};
use super::{App, dim, mint};
use crate::update;

/// How often to look again while fan-lite keeps running.
const RECHECK: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(String),
    Installing(String),
    Failed(String),
}

pub(super) enum UpdateMsg {
    Checked(Result<Option<String>, String>),
    /// The tag, and where the new exe went.
    Installed(String, Result<std::path::PathBuf, String>),
}

impl App {
    fn update_busy(&self) -> bool {
        matches!(self.update, UpdateState::Checking | UpdateState::Installing(_))
    }

    fn start_check(&mut self, ctx: &egui::Context) {
        if self.update_busy() {
            return;
        }
        self.update = UpdateState::Checking;
        self.update_checked = Some(Instant::now());
        let (tx, rx) = mpsc::channel();
        self.update_rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(UpdateMsg::Checked(update::check()));
            ctx.request_repaint();
        });
    }

    fn start_install(&mut self, tag: String, ctx: &egui::Context) {
        if self.update_busy() {
            return;
        }
        self.update = UpdateState::Installing(tag.clone());
        let (tx, rx) = mpsc::channel();
        self.update_rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let r = std::env::current_exe()
                .map_err(|e| e.to_string())
                .and_then(|exe| update::install(&tag, &exe));
            let _ = tx.send(UpdateMsg::Installed(tag, r));
            ctx.request_repaint();
        });
    }

    /// Called every frame (and every engine tick while hidden): the daily check, and results
    /// from the worker thread. After an install, start the new exe and quit.
    pub(super) fn poll_update(&mut self, ctx: &egui::Context) {
        if self.cfg.update_check && self.update_checked.is_none_or(|t| t.elapsed() >= RECHECK) {
            self.start_check(ctx);
        }
        let Some(msg) = self.update_rx.as_ref().and_then(|rx| rx.try_recv().ok()) else {
            return;
        };
        self.update_rx = None;
        self.update = match msg {
            UpdateMsg::Checked(Ok(Some(tag))) => UpdateState::Available(tag),
            UpdateMsg::Checked(Ok(None)) => UpdateState::UpToDate,
            UpdateMsg::Checked(Err(e)) => UpdateState::Failed(format!("Update check failed: {e}")),
            UpdateMsg::Installed(_, Err(e)) => UpdateState::Failed(format!("Update failed: {e}")),
            UpdateMsg::Installed(tag, Ok(new)) => self.restart_into(&tag, &new, ctx),
        };
    }

    /// Start the freshly installed exe and close this one. The new copy waits for this one to
    /// exit (and hand the fans back) before it takes over. If Windows refuses to start it
    /// (e.g. Smart App Control), the old exe is put back and keeps running.
    fn restart_into(&mut self, tag: &str, new: &std::path::Path, ctx: &egui::Context) -> UpdateState {
        match std::process::Command::new(new).arg("--updated").spawn() {
            Ok(_) => {
                self.quitting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                UpdateState::Installing(tag.to_owned())
            }
            Err(e) => {
                let kept = std::env::current_exe().is_ok_and(|exe| update::rollback(&exe, new).is_ok());
                UpdateState::Failed(if kept {
                    format!("Windows didn't start {tag} ({e}); still running v{}", update::CURRENT)
                } else {
                    format!("{tag} installed but didn't start ({e}); start FanLite again")
                })
            }
        }
    }

    /// Title bar: "Update vX.Y.Z" when there is one (click to install), "Updating…" meanwhile.
    pub(super) fn update_title(&mut self, ui: &mut egui::Ui) {
        match self.update.clone() {
            UpdateState::Available(tag) => {
                let r = ui.add(
                    egui::Label::new(
                        RichText::new(format!("Update {tag}"))
                            .font(theme::sans_med(12.0))
                            .color(AMBER),
                    )
                    .sense(Sense::click()),
                );
                if r.on_hover_text("Download it from GitHub, install it and restart fan-lite")
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    self.start_install(tag, ui.ctx());
                }
            }
            UpdateState::Installing(_) => {
                ui.label(RichText::new("Updating…").font(theme::sans(12.0)).color(LABEL));
            }
            _ => {}
        }
    }

    /// Settings drawer row: version, "Check now", automatic checks on/off, and the status.
    pub(super) fn version_row(&mut self, ui: &mut egui::Ui, key: impl Fn(&mut egui::Ui, &str)) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            key(ui, "Version");
            ui.label(mint(format!("v{}", update::CURRENT), theme::mono(13.0)));
            let ctx = ui.ctx().clone();
            match self.update.clone() {
                UpdateState::Available(tag) => {
                    if pill(ui, true, &format!("Update to {tag}")).clicked() {
                        self.start_install(tag, &ctx);
                    }
                }
                UpdateState::Checking => {
                    ui.label(dim("checking…", theme::sans(12.0)));
                }
                UpdateState::Installing(tag) => {
                    ui.label(dim(format!("installing {tag}…"), theme::sans(12.0)));
                }
                _ => {
                    if pill(ui, false, "Check now").clicked() {
                        self.start_check(&ctx);
                    }
                }
            }
            if pill(ui, self.cfg.update_check, "Auto")
                .on_hover_text("Look for a new version at start and once a day")
                .clicked()
            {
                self.cfg.update_check = !self.cfg.update_check;
                changed = true;
            }
        });
        match &self.update {
            UpdateState::UpToDate => {
                ui.label(dim("fan-lite is up to date", theme::sans(12.0)));
            }
            UpdateState::Failed(msg) => {
                ui.label(RichText::new(msg).font(theme::sans(12.0)).color(RED));
            }
            _ => {}
        }
        changed
    }
}
