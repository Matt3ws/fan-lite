//! `--listen-test`: a scripted, beep-marked sequence that makes one fan group loud at a time,
//! so you can hear which one is the noisy one. Diagnostic only: every output it touched is
//! handed back at the end (and on drop), and it aborts to 100 % if anything gets hot.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Diagnostics::Debug::Beep;

use crate::hw;

struct Target {
    label: &'static str,
    id: &'static str,
    /// Duty used for the "everything quiet" phases.
    quiet: f32,
}

const TARGETS: [Target; 3] = [
    Target {
        label: "Radiator fans",
        id: "kraken/fan",
        quiet: 20.0,
    },
    Target {
        label: "Case fans (\"Fans\")",
        id: "nct/pwm7",
        quiet: 20.0,
    },
    Target {
        label: "Pump",
        id: "kraken/pump",
        quiet: 30.0,
    },
];
/// Abort and run everything at 100 % if any temperature reaches this.
const CRITICAL_C: f32 = 90.0;

/// (backend index, control index, rpm input index) for a target that was found.
type Loc = (usize, usize, Option<usize>);

fn beep(times: u32) {
    for _ in 0..times {
        unsafe { Beep(880, 120) };
        std::thread::sleep(Duration::from_millis(80));
    }
}

/// Runs the sequence and returns a log of what was set and measured.
pub fn run() -> String {
    let mut log = String::new();
    let det = hw::detect_all();
    for d in &det.diagnostics {
        let _ = writeln!(log, "! {d}");
    }
    let mut backends = det.backends;
    let locs: Vec<Option<Loc>> = TARGETS
        .iter()
        .map(|t| {
            backends.iter().enumerate().find_map(|(b, be)| {
                be.controls()
                    .iter()
                    .position(|c| c.id == t.id)
                    .map(|i| (b, i, be.controls()[i].rpm_fan))
            })
        })
        .collect();
    for (t, l) in TARGETS.iter().zip(&locs) {
        let _ = writeln!(
            log,
            "{:<22} {}",
            t.label,
            if l.is_some() { "found" } else { "NOT FOUND (skipped)" }
        );
    }

    // (name, seconds, duty per target; None = leave as it is)
    let quiet: Vec<Option<f32>> = TARGETS.iter().map(|t| Some(t.quiet)).collect();
    let mut phases: Vec<(String, u64, Vec<Option<f32>>)> = vec![
        ("baseline (unchanged)".into(), 10, vec![None; TARGETS.len()]),
        ("ALL QUIET".into(), 15, quiet.clone()),
    ];
    for (k, t) in TARGETS.iter().enumerate() {
        let mut loud = quiet.clone();
        loud[k] = Some(100.0);
        phases.push((format!("{} 100%", t.label), 10, loud));
        phases.push(("all quiet".into(), 10, quiet.clone()));
    }

    let start = Instant::now();
    let mut touched = vec![false; TARGETS.len()];
    'phases: for (n, (name, secs, duties)) in phases.iter().enumerate() {
        beep(if n == 0 { 1 } else { 2 });
        for (k, d) in duties.iter().enumerate() {
            if let (Some(d), Some((b, i, _))) = (d, locs[k]) {
                match backends[b].set_duty(i, *d) {
                    Ok(()) => touched[k] = true,
                    Err(e) => {
                        let _ = writeln!(log, "  ! {}: {e}", TARGETS[k].label);
                    }
                }
            }
        }
        let mut last_rpm = vec![None; TARGETS.len()];
        let mut last_duty: Vec<Option<f32>> = vec![None; TARGETS.len()];
        let mut hottest: f32 = f32::MIN;
        for _ in 0..*secs {
            std::thread::sleep(Duration::from_secs(1));
            let mut rpm_by_backend = Vec::new();
            let mut duty_by_backend = Vec::new();
            for be in backends.iter_mut() {
                let mut t = vec![None; be.temps().len()];
                let mut f = vec![None; be.fans().len()];
                let mut d = vec![None; be.controls().len()];
                be.update(&mut t, &mut f, &mut d);
                hottest = t.iter().flatten().fold(hottest, |a, &v| a.max(v));
                rpm_by_backend.push(f);
                duty_by_backend.push(d);
            }
            for (k, l) in locs.iter().enumerate() {
                if let Some((b, i, f)) = l {
                    last_rpm[k] = f.and_then(|f| rpm_by_backend[*b][f]);
                    last_duty[k] = duty_by_backend[*b][*i];
                }
            }
            if hottest >= CRITICAL_C {
                let _ = writeln!(log, "! {hottest:.0} °C reached: everything to 100 %, test stopped");
                for (k, l) in locs.iter().enumerate() {
                    if let Some((b, i, _)) = l {
                        let _ = backends[*b].set_duty(*i, 100.0);
                        touched[k] = true;
                    }
                }
                std::thread::sleep(Duration::from_secs(5));
                break 'phases;
            }
        }
        let t = start.elapsed().as_secs();
        let rpms: Vec<String> = TARGETS
            .iter()
            .zip(last_rpm.iter().zip(&last_duty))
            .map(|(tg, (r, d))| {
                let rpm = r.map_or("--".into(), |r: f32| format!("{r:.0} rpm"));
                let duty = d.map_or(String::new(), |d: f32| format!(" ({d:.0}%)"));
                format!("{} {rpm}{duty}", tg.label)
            })
            .collect();
        let _ = writeln!(
            log,
            "[{:>2}:{:02}] {name:<28} | {}  (hottest {hottest:.0} °C)",
            t / 60,
            t % 60,
            rpms.join(" · ")
        );
    }

    for (k, l) in locs.iter().enumerate() {
        if let (true, Some((b, i, _))) = (touched[k], l)
            && let Err(e) = backends[*b].release(*i)
        {
            let _ = writeln!(log, "! could not hand back {}: {e}", TARGETS[k].label);
        }
    }
    beep(3);
    let _ = writeln!(log, "done: everything handed back");
    log
}
