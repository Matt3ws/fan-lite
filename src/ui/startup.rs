//! Settings row for Start with Windows. Task Scheduler calls run on short-lived threads, so the
//! UI never waits on `schtasks.exe`.

use std::path::Path;
use std::sync::mpsc;

use eframe::egui::{self, RichText};

use super::theme::{self, RED, pill};
use super::{App, dim};
use crate::{autostart, update};

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) enum StartupState {
    /// Not looked up yet.
    #[default]
    Unknown,
    Working,
    On,
    Off,
    Failed(String),
}

fn same_file(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
}

impl App {
    fn startup_job(&mut self, ctx: &egui::Context, job: impl FnOnce() -> StartupState + Send + 'static) {
        self.startup = StartupState::Working;
        let (tx, rx) = mpsc::channel();
        self.startup_rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(job());
            ctx.request_repaint();
        });
    }

    /// Look the task up once at start. If FanLite was moved since it was set up, point the
    /// task at this exe (a development build in `target` never takes the task over).
    pub(super) fn poll_startup(&mut self, ctx: &egui::Context) {
        if self.startup == StartupState::Unknown && self.startup_rx.is_none() {
            self.startup_job(ctx, || {
                let Some(task) = autostart::installed() else {
                    return StartupState::Off;
                };
                match std::env::current_exe() {
                    Ok(exe) if !same_file(&task, &exe) && !update::is_dev_build(&exe) => {
                        match autostart::enable(&exe) {
                            Ok(()) => StartupState::On,
                            Err(e) => StartupState::Failed(format!("Couldn't move Start with Windows here: {e}")),
                        }
                    }
                    _ => StartupState::On,
                }
            });
        }
        if let Some(s) = self.startup_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.startup_rx = None;
            self.startup = s;
        }
    }

    /// Settings drawer row: Start with Windows on/off.
    pub(super) fn startup_row(&mut self, ui: &mut egui::Ui, key: impl Fn(&mut egui::Ui, &str)) {
        let on = self.startup == StartupState::On;
        ui.horizontal(|ui| {
            key(ui, "Startup");
            let busy = matches!(self.startup, StartupState::Working | StartupState::Unknown);
            let r = pill(ui, on, "Start with Windows").on_hover_text(
                "Start FanLite in the notification area when you sign in, with admin rights and no \
                 UAC prompt (a Task Scheduler task named FanLite)",
            );
            if r.clicked() && !busy {
                let ctx = ui.ctx().clone();
                if on {
                    self.startup_job(&ctx, || match autostart::disable() {
                        Ok(()) => StartupState::Off,
                        Err(e) => StartupState::Failed(format!("Couldn't turn it off: {e}")),
                    });
                } else {
                    self.startup_job(&ctx, || {
                        let r = std::env::current_exe()
                            .map_err(|e| e.to_string())
                            .and_then(|exe| autostart::enable(&exe));
                        match r {
                            Ok(()) => StartupState::On,
                            Err(e) => StartupState::Failed(format!("Couldn't turn it on: {e}")),
                        }
                    });
                }
            }
            if busy {
                ui.label(dim("…", theme::sans(12.0)));
            }
        });
        if let StartupState::Failed(msg) = &self.startup {
            ui.label(RichText::new(msg).font(theme::sans(12.0)).color(RED));
        } else if on && std::env::current_exe().is_ok_and(|e| update::is_dev_build(&e)) {
            ui.label(dim(
                "This is a development build: Windows will start it from the target folder.",
                theme::sans(12.0),
            ));
        }
    }
}
