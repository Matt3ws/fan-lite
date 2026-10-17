//! Corsair iCUE Commander Core XT lighting over USB HID (interface MI_00).
//!
//! Protocol per liquidctl's `commander_core` notes and OpenRGB, checked against a Core XT
//! (firmware 1.4.62): 385-byte output reports `00 08 <cmd…>`; replies `00 00 <cmd> <status> …`.
//! - `01 03 00 02` wake (software mode) / `01 03 00 01` sleep (back to the hub's own lighting)
//! - `0d 00 <endpoint>` open, `05 01 00` close, `08 00 01` read (data type, then data)
//! - `06 00 <len lo> <len hi> 00 00 <type lo> <type hi> <data>` write, `07 00 <data>` continue
//! - endpoint `1a` (type `09 00`): fan ports, `07` = a fan is connected
//! - endpoint `20` (type `0f 00`): RGB ports, per port u16 state (`02` = connected) + u16 LEDs
//! - endpoint `22` (type `12 00`): LED colours, 408 R,G,B slots
//!
//! Lighting only works in software mode, so the hub shows fan-lite's colour while fan-lite
//! runs (re-sent every few seconds, or it falls back by itself) and its own stored lighting
//! after that. How the 408 slots map onto the ports isn't known yet, so the whole hub is one
//! zone with one colour.
//!
//! **Not enabled yet** (`hw::detect_all` doesn't use it): on a real Core XT only the first
//! port shows the colour, so the slot layout still has to be worked out.
//!
//! Software mode would also hand the hub's fan outputs to fan-lite, so the hub is only used
//! when no fan is connected to it.

use std::time::{Duration, Instant};

use crate::hw::hid::{self, Hid};
use crate::hw::win::process_running;
use crate::hw::{Backend, Channel, ControlChannel, HwError, LightZone};
use crate::rgb::{Effect, Kind, Rgb};

const VID: &str = "vid_1b1c";
const PID: &str = "pid_0c2a";
const INTERFACE: &str = "mi_00";
const REPORT: usize = 385;
const READ_TIMEOUT_MS: u32 = 500;
const MAX_READS: usize = 8;
/// Colour slots in the LED endpoint.
const SLOTS: usize = 408;
/// The hub leaves software mode on its own if nothing arrives for ~10 s.
const KEEPALIVE: Duration = Duration::from_secs(4);
const OWNER: &[&str] = &["icue"];
const OWNER_RECHECK: Duration = Duration::from_secs(5);

const WAKE: [u8; 4] = [0x01, 0x03, 0x00, 0x02];
const SLEEP: [u8; 4] = [0x01, 0x03, 0x00, 0x01];
const CLOSE: [u8; 3] = [0x05, 0x01, 0x00];
const READ: [u8; 3] = [0x08, 0x00, 0x01];

/// Connected RGB ports and their LED counts, from an endpoint `20` reply's data.
pub fn parse_leds(data: &[u8]) -> Vec<(usize, u16)> {
    let Some((&n, rest)) = data.split_first() else {
        return Vec::new();
    };
    rest.as_chunks::<4>()
        .0
        .iter()
        .take(n as usize)
        .enumerate()
        .filter(|(_, c)| c[0] == 0x02)
        .map(|(port, c)| (port, u16::from_le_bytes([c[2], c[3]])))
        .collect()
}

/// Whether any fan is connected, from an endpoint `1a` reply's data.
pub fn fans_connected(data: &[u8]) -> bool {
    data.split_first()
        .is_some_and(|(&n, rest)| rest.iter().take(n as usize).any(|&s| s == 0x07))
}

/// The packets that write `data` (with data type `ty`) to the open endpoint.
pub fn write_packets(ty: u8, data: &[u8], report: usize) -> Vec<Vec<u8>> {
    let len = (data.len() + 2) as u16;
    let mut first = vec![0x06, 0x00, len as u8, (len >> 8) as u8, 0x00, 0x00, ty, 0x00];
    // Report ID and the 0x08 prefix take two bytes of each report.
    let n = data.len().min(report - 2 - first.len());
    first.extend_from_slice(&data[..n]);
    let mut out = vec![first];
    for chunk in data[n..].chunks(report - 4) {
        let mut more = vec![0x07, 0x00];
        more.extend_from_slice(chunk);
        out.push(more);
    }
    out
}

pub struct CommanderCore {
    path: Vec<u16>,
    hid: Option<Hid>,
    zones: Vec<LightZone>,
    /// The colour being shown; `None` until fan-lite first sets one (the hub stays asleep).
    color: Option<Rgb>,
    last_sent: Option<Instant>,
    awake: bool,
    owner_checked: Option<Instant>,
    owner: Option<String>,
}

impl CommanderCore {
    /// `Ok(None)` if there is no Commander Core XT.
    pub fn detect() -> Result<Option<Self>, HwError> {
        let Some(dev) = hid::find(VID, Some(INTERFACE))
            .into_iter()
            .find(|d| d.lower.contains(PID))
        else {
            return Ok(None);
        };
        if let Some(o) = process_running(OWNER) {
            return Err(HwError(format!(
                "Commander Core XT found, but {o} is running - close iCUE to let fan-lite light it"
            )));
        }
        let hid = Hid::open(&dev.path, REPORT, "Corsair Commander Core XT")?;
        // Reading the ports needs software mode; go back to sleep right after.
        cmd(&hid, &WAKE)?;
        let fans = read(&hid, 0x1a);
        let leds = read(&hid, 0x20);
        let _ = cmd(&hid, &SLEEP);
        if fans_connected(&fans?) {
            return Err("Commander Core XT has fans connected; fan-lite leaves it to its own control".into());
        }
        let ports = parse_leds(&leds?);
        if ports.is_empty() {
            return Ok(None);
        }
        let total: u16 = ports.iter().map(|p| p.1).sum();
        Ok(Some(CommanderCore {
            path: dev.path,
            hid: Some(hid),
            zones: vec![LightZone {
                id: "corsair/hub".into(),
                label: format!("Corsair hub · {} devices", ports.len()),
                leds: total,
                kinds: vec![Kind::Off, Kind::Static],
            }],
            color: None,
            last_sent: None,
            awake: false,
            owner_checked: None,
            owner: None,
        }))
    }

    fn device(&mut self) -> Result<&Hid, HwError> {
        if self.owner_checked.is_none_or(|t| t.elapsed() >= OWNER_RECHECK) {
            self.owner_checked = Some(Instant::now());
            self.owner = process_running(OWNER);
        }
        if let Some(o) = &self.owner {
            self.awake = false;
            return Err(HwError(format!(
                "{o} controls the Corsair hub - close iCUE to use fan-lite"
            )));
        }
        if self.hid.is_none() {
            self.hid = Some(Hid::open(&self.path, REPORT, "Corsair Commander Core XT")?);
            self.awake = false;
        }
        Ok(self.hid.as_ref().unwrap())
    }

    /// Wake the hub if needed and send the current colour to every LED slot.
    fn send(&mut self) -> Result<(), HwError> {
        let Some(c) = self.color else { return Ok(()) };
        let wake = !self.awake;
        let data: Vec<u8> = (0..SLOTS).flat_map(|_| [c.0, c.1, c.2]).collect();
        let r = self.device().and_then(|h| {
            if wake {
                cmd(h, &WAKE)?;
            }
            cmd(h, &[0x0d, 0x00, 0x22])?;
            for p in write_packets(0x12, &data, REPORT) {
                cmd(h, &p)?;
            }
            cmd(h, &CLOSE).map(|_| ())
        });
        match &r {
            Ok(()) => {
                self.awake = true;
                self.last_sent = Some(Instant::now());
            }
            Err(_) => {
                self.hid = None; // re-open (and wake) next time
                self.awake = false;
            }
        }
        r
    }
}

/// Send one command and wait for its reply; fails on a non-zero status byte.
fn cmd(h: &Hid, c: &[u8]) -> Result<Vec<u8>, HwError> {
    let mut b = vec![0x00, 0x08];
    b.extend_from_slice(c);
    let r = h.request(&b, READ_TIMEOUT_MS, MAX_READS, |r| {
        (r.len() > 3 && r[2] == c[0]).then(|| r.to_vec())
    })?;
    match r[3] {
        0 => Ok(r),
        s => Err(HwError(format!(
            "Corsair hub rejected command {:02x} (status {s:02x})",
            c[0]
        ))),
    }
}

/// Read an endpoint's data (after its two data-type bytes).
fn read(h: &Hid, endpoint: u8) -> Result<Vec<u8>, HwError> {
    cmd(h, &[0x0d, 0x00, endpoint])?;
    let r = cmd(h, &READ);
    let _ = cmd(h, &CLOSE);
    Ok(r?.get(6..).unwrap_or_default().to_vec())
}

impl Backend for CommanderCore {
    fn name(&self) -> &'static str {
        "Corsair Commander Core XT"
    }
    fn temps(&self) -> &[Channel] {
        &[]
    }
    fn fans(&self) -> &[Channel] {
        &[]
    }
    fn controls(&self) -> &[ControlChannel] {
        &[]
    }

    /// Keepalive: re-send the colour before the hub drops out of software mode.
    fn update(&mut self, _: &mut [Option<f32>], _: &mut [Option<f32>], _: &mut [Option<f32>]) {
        if self.color.is_some() && self.last_sent.is_none_or(|t| t.elapsed() >= KEEPALIVE) {
            let _ = self.send();
        }
    }
    fn set_duty(&mut self, _: usize, _: f32) -> Result<(), HwError> {
        Err("no fan outputs".into())
    }
    fn release(&mut self, _: usize) -> Result<(), HwError> {
        Ok(())
    }

    fn lights(&self) -> &[LightZone] {
        &self.zones
    }

    fn set_light(&mut self, _: usize, e: &Effect) -> Result<(), HwError> {
        self.color = Some(match e.kind {
            Kind::Off => Rgb::BLACK,
            _ => e.colors[0].scaled(e.brightness),
        });
        self.send()
    }

    fn save_lights(&mut self) -> Result<(), HwError> {
        Err("the hub can't store lighting yet; it shows fan-lite's colour while fan-lite runs".into())
    }
}

impl Drop for CommanderCore {
    /// Back to the hub's own lighting.
    fn drop(&mut self) {
        if self.awake
            && let Some(h) = &self.hid
        {
            let _ = cmd(h, &SLEEP);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_replies() {
        // From a Core XT: 7 RGB ports, ports 1-5 have 16 LEDs, port 6 is empty.
        let leds = [
            0x07, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x10, 0x00, 0x02, 0x00, 0x10, 0x00, 0x02, 0x00, 0x10, 0x00, 0x02,
            0x00, 0x10, 0x00, 0x02, 0x00, 0x10, 0x00, 0x03, 0x00, 0x00, 0x00,
        ];
        assert_eq!(parse_leds(&leds), vec![(1, 16), (2, 16), (3, 16), (4, 16), (5, 16)]);
        assert!(parse_leds(&[]).is_empty());
        assert!(!fans_connected(&[0x06, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01]));
        assert!(fans_connected(&[0x06, 0x01, 0x07, 0x01, 0x01, 0x01, 0x01]));
        // Beyond the count doesn't count.
        assert!(!fans_connected(&[0x01, 0x01, 0x07]));
    }

    #[test]
    fn colour_write_packets() {
        let data = vec![0xAB; SLOTS * 3];
        let p = write_packets(0x12, &data, REPORT);
        let len = (SLOTS * 3 + 2) as u16;
        assert_eq!(
            &p[0][..8],
            &[0x06, 0x00, len as u8, (len >> 8) as u8, 0x00, 0x00, 0x12, 0x00]
        );
        assert!(p.iter().all(|x| x.len() + 2 <= REPORT), "every packet fits a report");
        assert!(p[1..].iter().all(|x| x[..2] == [0x07, 0x00]));
        let sent: usize = p[0].len() - 8 + p[1..].iter().map(|x| x.len() - 2).sum::<usize>();
        assert_eq!(sent, data.len());
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Read-only (wakes the hub only to read): what the driver detects.
    /// `cargo test --lib corsair_detect_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn corsair_detect_live() {
        match CommanderCore::detect() {
            Ok(Some(c)) => {
                for z in c.lights() {
                    println!("{} {} leds={}", z.id, z.label, z.leds);
                }
            }
            Ok(None) => println!("no hub"),
            Err(e) => println!("{e}"),
        }
    }

    /// Shows `FANLITE_RGB` (default 00ff00) through the driver for 15 s, with keepalives,
    /// then lets the hub go back to its own lighting.
    /// `cargo test --lib corsair_driver_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn corsair_driver_live() {
        let mut c = CommanderCore::detect().unwrap().expect("no hub");
        let rgb = std::env::var("FANLITE_RGB").unwrap_or_else(|_| "00ff00".into());
        let e = Effect {
            kind: Kind::Static,
            colors: [Rgb::parse_hex(&format!("#{rgb}")).expect("FANLITE_RGB=rrggbb"); 2],
            ..Default::default()
        };
        println!("set: {:?}", c.set_light(0, &e));
        for _ in 0..15 {
            std::thread::sleep(Duration::from_secs(1));
            c.update(&mut [], &mut [], &mut []);
        }
        drop(c);
        println!("back to the hub's own lighting");
    }
}
