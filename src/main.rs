#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fmt::Write as _;

use fan_lite::config::Config;
use fan_lite::{hw, ui};

fn main() {
    if std::env::args().any(|a| a == "--probe") {
        probe();
        return;
    }
    // One fan-lite at a time: two would fight over the fans. After an in-app update the new
    // copy waits here until the old one has handed the fans back and exited.
    let updated = std::env::args().any(|a| a == "--updated");
    // Started at sign-in (Start with Windows): only the tray icon, no window.
    let tray = std::env::args().any(|a| a == "--tray");
    let instance = hw::win::GlobalMutex::new(INSTANCE_MUTEX);
    let Some(_running) = instance.lock(if updated { 10_000 } else { 0 }) else {
        if !tray {
            already_running();
        }
        return;
    };
    if let Ok(exe) = std::env::current_exe() {
        fan_lite::update::clean_up_after_update(exe);
    }
    if std::env::args().any(|a| a == "--listen-test") {
        // Diagnostic: drive one fan group at a time so the user can hear which is loud.
        let log = fan_lite::listen::run();
        print!("{log}");
        if let Some(p) = Config::path().map(|p| p.with_file_name("listen-test.txt")) {
            let _ = p.parent().map(std::fs::create_dir_all);
            let _ = std::fs::write(&p, &log);
        }
        return;
    }

    let (cfg, cfg_errors) = Config::load();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("FanLite")
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(false)
            .with_inner_size([420.0, 600.0])
            .with_icon(std::sync::Arc::new(ui::window_icon())),
        renderer: eframe::Renderer::Glow,
        system_font_fallback: false,
        ..Default::default()
    };
    // The engine (and with it all hardware access) lives inside the app, so if the window
    // can't be created nothing is ever written to the fans.
    let _ = eframe::run_native(
        "fan-lite",
        options,
        Box::new(|cc| Ok(Box::new(ui::App::new(cc, cfg, cfg_errors, tray)))),
    );
}

/// Held for the life of the process (see `main`).
const INSTANCE_MUTEX: &str = "Local\\fan-lite.instance";

fn already_running() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW};
    let text = hw::win::wide("FanLite is already running. Find it in the notification area by the clock.");
    let title = hw::win::wide("FanLite");
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONINFORMATION,
        )
    };
}

/// Read-only hardware report: detect, read once, print. Never writes to hardware.
/// Printed to the parent console when there is one, and always saved next to the config
/// (an elevated process started from a normal shell has no console to print to).
fn probe() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };

    let mut out = String::new();
    let det = hw::detect_all();
    let _ = writeln!(
        out,
        "fan-lite {} probe (elevated: {})",
        env!("CARGO_PKG_VERSION"),
        hw::win::is_elevated()
    );
    for d in &det.diagnostics {
        let _ = writeln!(out, "  ! {d}");
    }
    let mut backends = det.backends;
    for b in &mut backends {
        let mut t = vec![None; b.temps().len()];
        let mut f = vec![None; b.fans().len()];
        let mut d = vec![None; b.controls().len()];
        const RUNS: u32 = 20;
        let start = std::time::Instant::now();
        for _ in 0..RUNS {
            b.update(&mut t, &mut f, &mut d);
        }
        let ms = start.elapsed().as_secs_f64() * 1000.0 / RUNS as f64;
        let _ = writeln!(out, "\n[{}]  read cost {ms:.2} ms", b.name());
        let fmt = |v: Option<f32>, unit: &str| v.map_or("--".to_owned(), |v| format!("{v:.1} {unit}"));
        for (c, v) in b.temps().iter().zip(&t) {
            let _ = writeln!(out, "  temp  {:<28} {:<18} {}", c.label, c.id, fmt(*v, "C"));
        }
        for (c, v) in b.fans().iter().zip(&f) {
            let _ = writeln!(out, "  fan   {:<28} {:<18} {}", c.label, c.id, fmt(*v, "rpm"));
        }
        for (c, v) in b.controls().iter().zip(&d) {
            let _ = writeln!(
                out,
                "  ctl   {:<28} {:<18} {} (hw min {:.0}%)",
                c.label,
                c.id,
                fmt(*v, "%"),
                c.hw_min_pct
            );
        }
        for z in b.lights() {
            let kinds: Vec<&str> = z.kinds.iter().map(|k| k.name()).collect();
            let _ = writeln!(
                out,
                "  light {:<28} {:<18} {} LEDs ({})",
                z.label,
                z.id,
                z.leds,
                kinds.join(", ")
            );
        }
    }
    print!("{out}");
    if let Some(p) = Config::path().map(|p| p.with_file_name("probe.txt")) {
        let _ = p.parent().map(std::fs::create_dir_all);
        let _ = std::fs::write(&p, &out);
        println!("\n(saved to {})", p.display());
    }
}
