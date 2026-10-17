//! Corsair iCUE Commander Core XT lighting over USB HID (interface MI_00).
//!
//! Protocol per liquidctl's `commander_core` notes, OpenRGB and SignalRGB's Core XT plugin,
//! checked against a Core XT (firmware 1.4.62). 385-byte output reports `00 08 <cmd> …`;
//! replies `00 00 <cmd> <status> …`. Endpoints are opened on one of three handles (0-2).
//! - `01 03 00 02` wake (software mode) / `01 03 00 01` sleep (back to the hub's own lighting)
//! - `0d <handle> <endpoint>` open, `05 01 <handle>` close, `08 <handle> 01` read
//! - `06 <handle> <len u32> <type u16> <data>` write, `07 <handle> <data>` continue
//! - `15 01` applies a changed LED configuration
//! - endpoint `1a` (type `09 00`): fan ports, `07` = a fan is connected
//! - endpoint `20` (type `0f 00`): RGB channels (3-pin strip, then ports 1-6), each a u16
//!   state (`02` = connected) and a u16 LED count
//! - endpoint `1e` (type `0d 00`): fan-port LED grouping; `1d` (type `0c 00`): strip length
//! - endpoint `22` (type `12 00`): colours, R,G,B per LED: the strip's LEDs, then each fan
//!   port as a 34-LED group (fans with fewer LEDs use the start of their group)
//!
//! Lighting only works in software mode: fan-lite configures the ports the way SignalRGB does,
//! keeps the colour endpoint open and re-sends the colours every few seconds. When fan-lite
//! exits the hub goes back to its own stored lighting. Colours are static (the hub runs no
//! effects of its own in software mode).
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
/// Fan ports, each a 34-LED group in the colour data.
const PORTS: usize = 6;
const PORT_SLOTS: usize = 34;
/// The hub leaves software mode on its own if nothing arrives for ~10 s.
const KEEPALIVE: Duration = Duration::from_secs(4);
const OWNER: &[&str] = &["icue"];
const OWNER_RECHECK: Duration = Duration::from_secs(5);

const WAKE: [u8; 4] = [0x01, 0x03, 0x00, 0x02];
const SLEEP: [u8; 4] = [0x01, 0x03, 0x00, 0x01];
const CLOSE: [u8; 3] = [0x05, 0x01, 0x00];
const READ: [u8; 3] = [0x08, 0x00, 0x01];
const CONFIRM: [u8; 2] = [0x15, 0x01];

/// Connected RGB channels (0 = 3-pin strip, 1-6 = fan ports) and their LED counts, from an
/// endpoint `20` reply's data.
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

/// The packets that write `data` (with data type `ty`) to the endpoint open on `handle`.
pub fn write_packets(handle: u8, ty: u8, data: &[u8], report: usize) -> Vec<Vec<u8>> {
    let len = (data.len() + 2) as u32;
    let mut first = vec![0x06, handle];
    first.extend_from_slice(&len.to_le_bytes());
    first.extend_from_slice(&[ty, 0x00]);
    // Report ID and the 0x08 prefix take two bytes of each report.
    let n = data.len().min(report - 2 - first.len());
    first.extend_from_slice(&data[..n]);
    let mut out = vec![first];
    for chunk in data[n..].chunks(report - 4) {
        let mut more = vec![0x07, handle];
        more.extend_from_slice(chunk);
        out.push(more);
    }
    out
}

/// Fan-port LED grouping: 7 entries of (count, type); the strip entry `01 01`, every fan
/// port one 34-LED ("QL") group `01 06`.
pub fn port_config() -> Vec<u8> {
    let mut c = vec![0x07, 0x01, 0x01];
    for _ in 0..PORTS {
        c.extend_from_slice(&[0x01, 0x06]);
    }
    c
}

/// Colour data: the strip's LEDs, then each fan port's 34-LED group. `colors[0]` is the
/// strip, `colors[1..=6]` the ports; unset channels are dark.
pub fn frame(strip_leds: u16, colors: &[Option<Rgb>; PORTS + 1]) -> Vec<u8> {
    let mut d = Vec::with_capacity((strip_leds as usize + PORTS * PORT_SLOTS) * 3);
    let fill = |d: &mut Vec<u8>, n: usize, c: Option<Rgb>| {
        let c = c.unwrap_or(Rgb::BLACK);
        d.extend((0..n).flat_map(|_| [c.0, c.1, c.2]));
    };
    fill(&mut d, strip_leds as usize, colors[0]);
    for c in &colors[1..] {
        fill(&mut d, PORT_SLOTS, *c);
    }
    d
}

pub struct CommanderCore {
    path: Vec<u16>,
    hid: Option<Hid>,
    zones: Vec<LightZone>,
    /// Channel (0 = strip, 1-6 = port) of each zone.
    channel: Vec<usize>,
    strip_leds: u16,
    colors: [Option<Rgb>; PORTS + 1],
    last_sent: Option<Instant>,
    /// Software mode is set up and the colour endpoint is open on handle 0.
    ready: bool,
    owner_checked: Option<Instant>,
    owner: Option<String>,
}

impl CommanderCore {
    /// `Ok(None)` if there is no Commander Core XT, or nothing is plugged into its RGB ports.
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
        let mut zones = Vec::new();
        let mut channel = Vec::new();
        let mut strip_leds = 0;
        for (ch, leds) in parse_leds(&leds?).into_iter().filter(|&(ch, n)| n > 0 && ch <= PORTS) {
            let (id, label) = if ch == 0 {
                strip_leds = leds;
                ("corsair/strip".to_owned(), "Corsair LED strip".to_owned())
            } else {
                (format!("corsair/port{ch}"), format!("Corsair port {ch}"))
            };
            zones.push(LightZone {
                id,
                label,
                leds,
                kinds: vec![Kind::Off, Kind::Static],
            });
            channel.push(ch);
        }
        if zones.is_empty() {
            return Ok(None);
        }
        Ok(Some(CommanderCore {
            path: dev.path,
            hid: Some(hid),
            zones,
            channel,
            strip_leds,
            colors: [None; PORTS + 1],
            last_sent: None,
            ready: false,
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
            self.ready = false;
            return Err(HwError(format!(
                "{o} controls the Corsair hub - close iCUE to use fan-lite"
            )));
        }
        if self.hid.is_none() {
            self.hid = Some(Hid::open(&self.path, REPORT, "Corsair Commander Core XT")?);
            self.ready = false;
        }
        Ok(self.hid.as_ref().unwrap())
    }

    /// Software mode, port grouping, strip length, then the colour endpoint on handle 0.
    fn setup(h: &Hid, strip_leds: u16) -> Result<(), HwError> {
        cmd(h, &WAKE)?;
        for handle in 0..3 {
            let _ = cmd(h, &[0x05, 0x01, handle]); // whatever was left open
        }
        cmd(h, &[0x0d, 0x01, 0x1e])?;
        for p in write_packets(0x01, 0x0d, &port_config(), REPORT) {
            cmd(h, &p)?;
        }
        cmd(h, &[0x0d, 0x02, 0x1d])?;
        // Same 15-byte size as the port grouping; a shorter payload shifts the colour data.
        let mut strip = [0u8; 15];
        strip[..3].copy_from_slice(&[0x07, strip_leds as u8, (strip_leds >> 8) as u8]);
        for p in write_packets(0x02, 0x0c, &strip, REPORT) {
            cmd(h, &p)?;
        }
        cmd(h, &[0x05, 0x01, 0x02])?;
        cmd(h, &[0x05, 0x01, 0x01])?;
        cmd(h, &CONFIRM)?;
        cmd(h, &[0x0d, 0x00, 0x22]).map(|_| ())
    }

    /// Send the current colours (setting the hub up first if needed).
    fn send(&mut self) -> Result<(), HwError> {
        if self.colors.iter().all(Option::is_none) {
            return Ok(());
        }
        let data = frame(self.strip_leds, &self.colors);
        let (ready, strip) = (self.ready, self.strip_leds);
        let r = self.device().and_then(|h| {
            if !ready {
                Self::setup(h, strip)?;
            }
            write_packets(0x00, 0x12, &data, REPORT)
                .iter()
                .try_for_each(|p| cmd(h, p).map(|_| ()))
        });
        match &r {
            Ok(()) => {
                self.ready = true;
                self.last_sent = Some(Instant::now());
            }
            Err(_) => {
                self.hid = None; // re-open and set up again next time
                self.ready = false;
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

/// Read an endpoint's data (after its two data-type bytes), on handle 0.
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

    /// Keepalive: re-send the colours before the hub drops out of software mode, and retry
    /// after a failure (e.g. once iCUE has been closed).
    fn update(&mut self, _: &mut [Option<f32>], _: &mut [Option<f32>], _: &mut [Option<f32>]) {
        if self.last_sent.is_none_or(|t| t.elapsed() >= KEEPALIVE) {
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

    fn set_light(&mut self, i: usize, e: &Effect) -> Result<(), HwError> {
        self.colors[self.channel[i]] = Some(match e.kind {
            Kind::Off => Rgb::BLACK,
            _ => e.colors[0].scaled(e.brightness),
        });
        self.send()
    }

    fn save_lights(&mut self) -> Result<(), HwError> {
        Err("the Corsair hub can't store lighting; it shows fan-lite's colours while fan-lite runs".into())
    }
}

impl Drop for CommanderCore {
    /// Back to the hub's own lighting.
    fn drop(&mut self) {
        if self.ready
            && let Some(h) = &self.hid
        {
            let _ = cmd(h, &CLOSE);
            let _ = cmd(h, &SLEEP);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_replies() {
        // From a Core XT: 7 RGB channels, ports 1-5 have 16 LEDs, port 6 is empty.
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
    fn setup_payloads() {
        assert_eq!(
            port_config(),
            [
                0x07, 0x01, 0x01, 0x01, 0x06, 0x01, 0x06, 0x01, 0x06, 0x01, 0x06, 0x01, 0x06, 0x01, 0x06
            ]
        );
        let p = write_packets(0x01, 0x0d, &port_config(), REPORT);
        assert_eq!(p.len(), 1);
        assert_eq!(&p[0][..8], &[0x06, 0x01, 17, 0, 0, 0, 0x0d, 0x00]);
    }

    #[test]
    fn colour_frame_layout() {
        let mut colors = [None; PORTS + 1];
        colors[2] = Some(Rgb(1, 2, 3));
        let f = frame(10, &colors);
        assert_eq!(f.len(), (10 + PORTS * PORT_SLOTS) * 3);
        // Port 2 starts after the strip's 10 LEDs and port 1's 34-LED group.
        let o = (10 + PORT_SLOTS) * 3;
        assert_eq!(&f[o..o + 3], &[1, 2, 3]);
        assert_eq!(&f[o + 33 * 3..o + 34 * 3], &[1, 2, 3]);
        assert!(f[..o].iter().all(|&b| b == 0) && f[o + 34 * 3..].iter().all(|&b| b == 0));
    }

    #[test]
    fn colour_write_packets() {
        let data = frame(0, &[Some(Rgb(9, 9, 9)); PORTS + 1]);
        let p = write_packets(0x00, 0x12, &data, REPORT);
        let len = (data.len() + 2) as u32;
        let mut head = vec![0x06, 0x00];
        head.extend_from_slice(&len.to_le_bytes());
        head.extend_from_slice(&[0x12, 0x00]);
        assert_eq!(&p[0][..8], &head[..]);
        assert!(p.iter().all(|x| x.len() + 2 <= REPORT), "every packet fits a report");
        assert!(p[1..].iter().all(|x| x[..2] == [0x07, 0x00]));
        let sent: usize = p[0].len() - 8 + p[1..].iter().map(|x| x.len() - 2).sum::<usize>();
        assert_eq!(sent, data.len());
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// What the driver detects (wakes the hub only to read the ports).
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

    /// Through the driver: zone k gets green, blue, white, yellow, cyan, magenta for 20 s with
    /// keepalives, then the hub goes back to its own lighting.
    /// `cargo test --lib corsair_driver_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn corsair_driver_live() {
        let mut c = CommanderCore::detect().unwrap().expect("no hub");
        let palette = [
            Rgb(0, 255, 0),
            Rgb(0, 0, 255),
            Rgb(255, 255, 255),
            Rgb(255, 200, 0),
            Rgb(0, 255, 255),
            Rgb(255, 0, 255),
        ];
        for i in 0..c.lights().len() {
            let e = Effect {
                kind: Kind::Static,
                colors: [palette[i % palette.len()]; 2],
                ..Default::default()
            };
            let r = c.set_light(i, &e);
            println!("{}: {r:?}", c.lights()[i].id);
        }
        for _ in 0..20 {
            std::thread::sleep(Duration::from_secs(1));
            c.update(&mut [], &mut [], &mut []);
        }
        drop(c);
        println!("back to the hub's own lighting");
    }
}
