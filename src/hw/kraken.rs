//! NZXT Kraken AIO coolers (Kraken 2023, Kraken Elite 2023, Kraken Z3) over USB HID:
//! liquid temperature, pump and radiator-fan speed and control.
//!
//! Protocol as implemented by liquidctl (`liquidctl/driver/kraken3.py`) and the Linux
//! `nzxt-kraken3` hwmon driver. 64-byte reports whose first byte is the report ID. The device
//! evaluates speed *curves* itself against its liquid temperature (20..=59 °C), so a fixed
//! duty is sent as a flat curve. Only one program may talk to it at a time (not with NZXT CAM).

use std::time::{Duration, Instant};

use super::hid::{self, Hid};
use super::win::process_running;
use super::{Backend, Channel, ControlChannel, HwError};

const VID: &str = "vid_1e71";
/// Models that speak this protocol: (lower-case PID in the device path, name).
const MODELS: [(&str, &str); 3] = [
    ("pid_300c", "NZXT Kraken Elite"),
    ("pid_300e", "NZXT Kraken"),
    ("pid_3008", "NZXT Kraken Z3"),
];
/// The control/status interface; MI_00 is the LCD's bulk interface.
const HID_INTERFACE: &str = "mi_01";
const REPORT: usize = 64;
const READ_TIMEOUT_MS: u32 = 150;
const MAX_READS: usize = 12;
/// The firmware can lock up or drop changes if curves arrive too fast.
const MIN_CURVE_GAP: Duration = Duration::from_millis(250);
/// How often to check whether NZXT CAM has started (it would fight us for the device).
const CAM_RECHECK: Duration = Duration::from_secs(5);
/// Lowest pump duty the protocol allows.
pub const PUMP_MIN: f32 = 20.0;

/// Curves the Kraken runs on its own (against liquid °C) when fan-lite hands a channel back.
const PUMP_RELEASE: [(f32, f32); 4] = [(30.0, 40.0), (35.0, 60.0), (40.0, 80.0), (45.0, 100.0)];
const FAN_RELEASE: [(f32, f32); 4] = [(30.0, 30.0), (35.0, 45.0), (40.0, 70.0), (45.0, 100.0)];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Status {
    /// `None` when the firmware reports a fault (`FF FF`: often missing SATA power).
    pub liquid: Option<f32>,
    pub pump_rpm: f32,
    pub pump_duty: f32,
    pub fan_rpm: f32,
    pub fan_duty: f32,
}

/// Decode a status reply (`75 01 …`). `None` for any other report.
pub fn parse_status(b: &[u8]) -> Option<Status> {
    if b.len() < 26 || b[0] != 0x75 || b[1] != 0x01 {
        return None;
    }
    let liquid = (!(b[15] == 0xFF && b[16] == 0xFF)).then(|| b[15] as f32 + b[16] as f32 / 10.0);
    Some(Status {
        liquid,
        pump_rpm: u16::from_le_bytes([b[17], b[18]]) as f32,
        pump_duty: b[19] as f32,
        fan_rpm: u16::from_le_bytes([b[23], b[24]]) as f32,
        fan_duty: b[25] as f32,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chan {
    Pump,
    Fan,
}

impl Chan {
    fn min(self) -> f32 {
        match self {
            Chan::Pump => PUMP_MIN,
            Chan::Fan => 0.0,
        }
    }
}

/// Full speed-curve command for liquid 20..=59 °C, padded to one report.
pub fn curve_cmd(chan: Chan, duties: &[u8; 40]) -> [u8; REPORT] {
    let mut b = [0u8; REPORT];
    // Header bytes 2..4 are specific to the 2023 models (0x00 0x00 on the Z3 also works for the pump).
    let header: [u8; 4] = match chan {
        Chan::Pump => [0x72, 0x01, 0x01, 0x00],
        Chan::Fan => [0x72, 0x02, 0x01, 0x01],
    };
    b[..4].copy_from_slice(&header);
    b[4..44].copy_from_slice(duties);
    b
}

/// A fixed duty as a flat curve, with the device-side failsafe of 100 % at 59 °C liquid.
pub fn flat_curve(chan: Chan, pct: f32) -> [u8; 40] {
    let d = pct.clamp(chan.min(), 100.0).round() as u8;
    let mut c = [d; 40];
    c[39] = 100;
    c
}

/// Interpolate (°C, %) points onto the device's 20..=59 °C grid: flat beyond the ends,
/// never decreasing, clamped to the channel's range, and 100 % at 59 °C.
pub fn liquid_curve(chan: Chan, points: &[(f32, f32)]) -> [u8; 40] {
    let mut out = [0u8; 40];
    let mut max = 0u8;
    for (i, slot) in out.iter_mut().enumerate() {
        let t = 20.0 + i as f32;
        let v = match points.iter().position(|p| p.0 >= t) {
            None => points.last().map_or(100.0, |p| p.1),
            Some(0) => points[0].1,
            Some(j) => {
                let (a, b) = (points[j - 1], points[j]);
                a.1 + (b.1 - a.1) * (t - a.0) / (b.0 - a.0)
            }
        };
        let v = v.clamp(chan.min(), 100.0).round() as u8;
        max = max.max(v);
        *slot = max;
    }
    out[39] = 100;
    out
}

/// True if NZXT CAM is running: it owns the device and would fight over it.
fn cam_running() -> bool {
    process_running(&["nzxt cam"]).is_some()
}

/// Device path and model name of the first supported Kraken's control interface.
fn find_device() -> Option<(Vec<u16>, &'static str)> {
    hid::find(VID, Some(HID_INTERFACE)).into_iter().find_map(|d| {
        MODELS
            .iter()
            .find(|(pid, _)| d.lower.contains(pid))
            .map(|&(_, model)| (d.path, model))
    })
}

fn open(path: &[u16]) -> Result<Hid, HwError> {
    let hid = Hid::open(path, REPORT, "Kraken")?;
    hid.flush();
    hid.write(&[0x70, 0x02, 0x01, 0xB8, 0x01])?;
    hid.write(&[0x70, 0x01])?;
    Ok(hid)
}

fn status(hid: &Hid) -> Result<Status, HwError> {
    hid.request(&[0x74, 0x01], READ_TIMEOUT_MS, MAX_READS, parse_status)
}

pub struct Kraken {
    path: Vec<u16>,
    model: &'static str,
    hid: Option<Hid>,
    temps: Vec<Channel>,
    fans: Vec<Channel>,
    controls: Vec<ControlChannel>,
    /// Last curve sent on *any* channel: back-to-back pump and fan curves can make the
    /// firmware drop one, so the gap applies across channels.
    last_curve: Option<Instant>,
    /// Channels fan-lite has written to (and must hand back on release).
    taken: [bool; 2],
    /// When NZXT CAM was last looked for, and whether it was running then.
    cam_checked: Instant,
    cam: bool,
}

const CHANS: [Chan; 2] = [Chan::Pump, Chan::Fan];

impl Kraken {
    pub fn detect() -> Result<Self, HwError> {
        let Some((path, model)) = find_device() else {
            return Err("no NZXT Kraken found".into());
        };
        if cam_running() {
            return Err(HwError(format!(
                "{model} found, but NZXT CAM is running - close it to let fan-lite control it"
            )));
        }
        let hid = open(&path)?;
        status(&hid)?;
        Ok(Kraken {
            path,
            model,
            hid: Some(hid),
            temps: vec![Channel {
                id: "kraken/liquid".into(),
                label: "Liquid".into(),
            }],
            fans: vec![
                Channel {
                    id: "kraken/pump/rpm".into(),
                    label: "Pump".into(),
                },
                Channel {
                    id: "kraken/fan/rpm".into(),
                    label: "Radiator fans".into(),
                },
            ],
            controls: vec![
                ControlChannel {
                    id: "kraken/pump".into(),
                    label: "Pump".into(),
                    hw_min_pct: PUMP_MIN,
                    rpm_fan: Some(0),
                    optional: false,
                },
                ControlChannel {
                    id: "kraken/fan".into(),
                    label: "Radiator fans".into(),
                    hw_min_pct: 0.0,
                    rpm_fan: Some(1),
                    optional: true,
                },
            ],
            last_curve: None,
            taken: [false; 2],
            cam_checked: Instant::now(),
            cam: false,
        })
    }

    /// The open device, re-opening and re-initialising it if needed (e.g. after sleep).
    fn device(&mut self) -> Result<&Hid, HwError> {
        // CAM may start after us (e.g. at login). Two programs talking to the Kraken steal each
        // other's replies and overwrite each other's curves, so back off while it runs.
        if self.cam_checked.elapsed() >= CAM_RECHECK {
            self.cam_checked = Instant::now();
            self.cam = cam_running();
        }
        if self.cam {
            self.hid = None;
            return Err("NZXT CAM is running - close it to let fan-lite control the Kraken".into());
        }
        if self.hid.is_none() {
            let hid = open(&self.path)?;
            self.hid = Some(hid);
        }
        Ok(self.hid.as_ref().unwrap())
    }

    fn send_curve(&mut self, i: usize, duties: &[u8; 40]) -> Result<(), HwError> {
        if let Some(t) = self.last_curve {
            let since = t.elapsed();
            if since < MIN_CURVE_GAP {
                std::thread::sleep(MIN_CURVE_GAP - since);
            }
        }
        let cmd = curve_cmd(CHANS[i], duties);
        let r = self.device().and_then(|h| h.write(&cmd));
        self.last_curve = Some(Instant::now());
        if r.is_err() {
            self.hid = None; // re-open next time
        }
        r
    }
}

impl Backend for Kraken {
    fn name(&self) -> &'static str {
        self.model
    }
    fn temps(&self) -> &[Channel] {
        &self.temps
    }
    fn fans(&self) -> &[Channel] {
        &self.fans
    }
    fn controls(&self) -> &[ControlChannel] {
        &self.controls
    }

    fn update(&mut self, temps: &mut [Option<f32>], fans: &mut [Option<f32>], duty: &mut [Option<f32>]) {
        temps.fill(None);
        fans.fill(None);
        duty.fill(None);
        let status = self.device().and_then(status);
        match status {
            Ok(s) => {
                temps[0] = s.liquid;
                fans[0] = Some(s.pump_rpm);
                fans[1] = Some(s.fan_rpm);
                duty[0] = Some(s.pump_duty);
                duty[1] = Some(s.fan_duty);
            }
            Err(_) => self.hid = None, // missed sample; re-open and re-init next tick
        }
    }

    fn set_duty(&mut self, i: usize, pct: f32) -> Result<(), HwError> {
        self.send_curve(i, &flat_curve(CHANS[i], pct))?;
        self.taken[i] = true;
        Ok(())
    }

    /// There is no firmware "auto" to return to, so leave the Kraken running its own
    /// liquid-temperature curve.
    fn release(&mut self, i: usize) -> Result<(), HwError> {
        if !self.taken[i] {
            return Ok(());
        }
        let pts: &[(f32, f32)] = if CHANS[i] == Chan::Pump {
            &PUMP_RELEASE
        } else {
            &FAN_RELEASE
        };
        self.send_curve(i, &liquid_curve(CHANS[i], pts))?;
        self.taken[i] = false;
        Ok(())
    }
}

impl Drop for Kraken {
    fn drop(&mut self) {
        for i in 0..CHANS.len() {
            let _ = self.release(i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn status_from_liquidctl_sample() {
        // Sample reply from liquidctl's tests (tests/test_kraken3.py).
        let mut b = hex("75012E0018001051393434363731011803690314140102000000");
        b.resize(64, 0);
        let s = parse_status(&b).unwrap();
        assert_eq!(s.liquid, Some(24.3));
        assert_eq!(s.pump_rpm, 873.0);
        assert_eq!(s.pump_duty, 20.0);
        assert_eq!((s.fan_rpm, s.fan_duty), (0.0, 0.0));
    }

    #[test]
    fn status_fault_and_wrong_report() {
        let mut b = vec![0u8; 64];
        b[0] = 0x75;
        b[1] = 0x01;
        b[15] = 0xFF;
        b[16] = 0xFF;
        assert_eq!(parse_status(&b).unwrap().liquid, None, "FF FF = sensor fault");
        b[1] = 0x02;
        assert_eq!(parse_status(&b), None);
        assert_eq!(parse_status(&[0x75, 0x01]), None, "too short");
    }

    #[test]
    fn speed_commands() {
        let p = curve_cmd(Chan::Pump, &flat_curve(Chan::Pump, 84.0));
        assert_eq!(p.len(), 64);
        assert_eq!(&p[..4], &[0x72, 0x01, 0x01, 0x00]);
        assert!(p[4..43].iter().all(|&d| d == 84));
        assert_eq!(p[43], 100, "failsafe at 59 °C");
        assert!(p[44..].iter().all(|&d| d == 0));
        let f = curve_cmd(Chan::Fan, &flat_curve(Chan::Fan, 0.0));
        assert_eq!(&f[..4], &[0x72, 0x02, 0x01, 0x01]);
        assert_eq!(f[4], 0, "fans may stop");
        assert_eq!(flat_curve(Chan::Pump, 10.0)[0], 20, "pump never below 20 %");
    }

    #[test]
    fn release_curves_are_sane() {
        for (chan, pts) in [(Chan::Pump, &PUMP_RELEASE[..]), (Chan::Fan, &FAN_RELEASE[..])] {
            let c = liquid_curve(chan, pts);
            assert!(c.windows(2).all(|w| w[0] <= w[1]), "monotonic");
            assert_eq!(c[39], 100);
            assert!(c[0] as f32 >= chan.min());
        }
        let pump = liquid_curve(Chan::Pump, &PUMP_RELEASE);
        assert_eq!(pump[10], 40, "30 °C liquid -> 40 %");
        assert_eq!(pump[25], 100, "45 °C liquid -> 100 %");
    }

    /// Read-only: firmware info and the lighting-accessory list, as hex.
    /// `cargo test --lib kraken_rgb_probe -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn kraken_rgb_probe() {
        let (path, model) = find_device().expect("no Kraken");
        let hid = open(&path).unwrap();
        let hexs = |r: &[u8]| r.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ");
        println!("{model}");
        for (req, want) in [([0x10u8, 0x01], [0x11u8, 0x01]), ([0x20, 0x03], [0x21, 0x03])] {
            let r = hid.request(&req, 300, 12, |r| (r.len() > 2 && r[..2] == want).then(|| r.to_vec()));
            match r {
                Ok(r) => println!("{:02x?} -> {}", req, hexs(&r)),
                Err(e) => println!("{:02x?} -> {e}", req),
            }
        }
    }
}
