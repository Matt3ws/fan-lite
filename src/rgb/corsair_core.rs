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
//! Lighting only works in software mode, and the hub runs no effects of its own there, so a
//! worker thread renders them: ~20 frames a second while an animated effect is on, otherwise
//! one frame on each change plus a keepalive every few seconds. fan-lite configures the ports
//! the way SignalRGB does. When fan-lite exits the hub goes back to its own stored lighting.
//!
//! Software mode would also hand the hub's fan outputs to fan-lite, so the hub is only used
//! when no fan is connected to it.

use std::f32::consts::TAU;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::hw::hid::{self, Hid};
use crate::hw::win::process_running;
use crate::hw::{Backend, Channel, ControlChannel, HwError, LightZone};
use crate::rgb::{Effect, Kind, Rgb, Speed};

const VID: &str = "vid_1b1c";
const PID: &str = "pid_0c2a";
const INTERFACE: &str = "mi_00";
const REPORT: usize = 385;
const READ_TIMEOUT_MS: u32 = 500;
const MAX_READS: usize = 8;
/// Fan ports, each a 34-LED group in the colour data.
const PORTS: usize = 6;
const PORT_SLOTS: usize = 34;
/// Channels: the 3-pin strip, then the fan ports.
const CHANNELS: usize = PORTS + 1;
/// The hub leaves software mode on its own if nothing arrives for ~10 s.
const KEEPALIVE: Duration = Duration::from_secs(4);
/// Frame interval while an effect animates.
const FRAME: Duration = Duration::from_millis(50);
/// After an error (iCUE running, device gone), wait this long before trying again.
const RETRY: Duration = Duration::from_secs(5);
const OWNER: &[&str] = &["icue"];

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

fn animated(e: &Effect) -> bool {
    matches!(e.kind, Kind::Breathing | Kind::ColorCycle | Kind::Rainbow | Kind::Wave)
}

/// Seconds per cycle of an effect.
fn period(e: &Effect) -> f32 {
    let (slow, normal, fast) = match e.kind {
        Kind::Breathing => (6.0, 4.0, 2.0),
        _ => (12.0, 6.0, 3.0),
    };
    match e.speed {
        Speed::Slow => slow,
        Speed::Normal => normal,
        Speed::Fast => fast,
    }
}

/// Colour of LED `led` of `n` on channel `ch` at time `t` seconds.
pub fn led_color(e: &Effect, ch: usize, led: usize, n: usize, t: f32) -> Rgb {
    let phase = t / period(e);
    let c = match e.kind {
        Kind::Off => Rgb::BLACK,
        Kind::Static => e.colors[0],
        Kind::Breathing => {
            // Fade in and out; every other breath in the second colour.
            let level = 0.5 - 0.5 * (phase.fract() * TAU).cos();
            e.colors[(phase as u64 % 2) as usize].dimmed(level)
        }
        Kind::ColorCycle => Rgb::hue(phase),
        Kind::Rainbow => Rgb::hue(phase + led as f32 / n.max(1) as f32),
        // One rainbow running across all channels, a fan's worth of hues per channel.
        Kind::Wave => Rgb::hue(phase - (ch as f32 + led as f32 / n.max(1) as f32) / CHANNELS as f32),
    };
    c.scaled(e.brightness)
}

/// Colour data at time `t`: the strip's LEDs, then each fan port's 34-LED group. `leds[k]` is
/// channel k's real LED count; unset channels are dark.
pub fn render(leds: &[u16; CHANNELS], fx: &[Option<Effect>; CHANNELS], t: f32) -> Vec<u8> {
    let mut d = Vec::with_capacity((leds[0] as usize + PORTS * PORT_SLOTS) * 3);
    for ch in 0..CHANNELS {
        let slots = if ch == 0 { leds[0] as usize } else { PORT_SLOTS };
        let n = if leds[ch] > 0 { leds[ch] as usize } else { slots };
        for led in 0..slots {
            let c = fx[ch].map_or(Rgb::BLACK, |e| led_color(&e, ch, led % n.max(1), n, t));
            d.extend_from_slice(&[c.0, c.1, c.2]);
        }
    }
    d
}

/// What the engine asks for, shared with the worker thread.
#[derive(Default)]
struct Shared {
    fx: [Option<Effect>; CHANNELS],
    stop: bool,
    /// Why the last frame couldn't be sent, if it couldn't.
    error: Option<String>,
}

type SharedState = Arc<(Mutex<Shared>, Condvar)>;

pub struct CommanderCore {
    zones: Vec<LightZone>,
    /// Channel (0 = strip, 1-6 = port) of each zone.
    channel: Vec<usize>,
    shared: SharedState,
    worker: Option<JoinHandle<()>>,
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
        let leds_reply = read(&hid, 0x20);
        let _ = cmd(&hid, &SLEEP);
        if fans_connected(&fans?) {
            return Err("Commander Core XT has fans connected; fan-lite leaves it to its own control".into());
        }
        let mut zones = Vec::new();
        let mut channel = Vec::new();
        let mut leds = [0u16; CHANNELS];
        for (ch, n) in parse_leds(&leds_reply?)
            .into_iter()
            .filter(|&(ch, n)| n > 0 && ch < CHANNELS)
        {
            leds[ch] = n;
            let (id, label) = if ch == 0 {
                ("corsair/strip".to_owned(), "Corsair LED strip".to_owned())
            } else {
                (format!("corsair/port{ch}"), format!("Corsair port {ch}"))
            };
            zones.push(LightZone {
                id,
                label,
                leds: n,
                kinds: Kind::ALL.to_vec(),
            });
            channel.push(ch);
        }
        if zones.is_empty() {
            return Ok(None);
        }
        let shared: SharedState = Arc::default();
        let state = shared.clone();
        let path = dev.path;
        let worker = std::thread::Builder::new()
            .name("corsair-lighting".into())
            .spawn(move || worker(path, hid, leds, state))
            .map_err(|e| HwError(e.to_string()))?;
        Ok(Some(CommanderCore {
            zones,
            channel,
            shared,
            worker: Some(worker),
        }))
    }
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

/// The lighting thread: idle until an effect is set, then keep the hub in software mode and
/// send frames until told to stop, then hand the hub back to its own lighting.
fn worker(path: Vec<u16>, hid: Hid, leds: [u16; CHANNELS], state: SharedState) {
    let (lock, cv) = &*state;
    let mut hid = Some(hid);
    let mut ready = false;
    let start = Instant::now();
    // Looking for iCUE walks the process list: not on every frame.
    let mut owner_checked: Option<Instant> = None;
    let mut owner = None;
    loop {
        let fx = {
            let mut s = lock.lock().unwrap_or_else(|e| e.into_inner());
            while !s.stop && s.fx.iter().all(Option::is_none) {
                s = cv.wait(s).unwrap_or_else(|e| e.into_inner());
            }
            if s.stop {
                break;
            }
            s.fx
        };
        if owner_checked.is_none_or(|t| t.elapsed() >= RETRY) {
            owner_checked = Some(Instant::now());
            owner = process_running(OWNER);
        }
        let r = (|| {
            if let Some(o) = &owner {
                return Err(HwError(format!(
                    "{o} controls the Corsair hub - close iCUE to use fan-lite"
                )));
            }
            if hid.is_none() {
                hid = Some(Hid::open(&path, REPORT, "Corsair Commander Core XT")?);
            }
            let h = hid.as_ref().unwrap();
            if !ready {
                setup(h, leds[0])?;
                ready = true;
            }
            let data = render(&leds, &fx, start.elapsed().as_secs_f32());
            write_packets(0x00, 0x12, &data, REPORT)
                .iter()
                .try_for_each(|p| cmd(h, p).map(|_| ()))
        })();
        let (error, wait) = match r {
            Ok(()) if fx.iter().flatten().any(animated) => (None, FRAME),
            Ok(()) => (None, KEEPALIVE),
            Err(e) => {
                // Re-open and set up again next time.
                hid = None;
                ready = false;
                (Some(e.to_string()), RETRY)
            }
        };
        let mut s = lock.lock().unwrap_or_else(|e| e.into_inner());
        s.error = error;
        if !s.stop {
            // Wakes early when the engine sets a new effect.
            s = cv.wait_timeout(s, wait).unwrap_or_else(|e| e.into_inner()).0;
        }
        drop(s);
    }
    if ready && let Some(h) = &hid {
        let _ = cmd(h, &CLOSE);
        let _ = cmd(h, &SLEEP);
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
    fn update(&mut self, _: &mut [Option<f32>], _: &mut [Option<f32>], _: &mut [Option<f32>]) {}
    fn set_duty(&mut self, _: usize, _: f32) -> Result<(), HwError> {
        Err("no fan outputs".into())
    }
    fn release(&mut self, _: usize) -> Result<(), HwError> {
        Ok(())
    }

    fn lights(&self) -> &[LightZone] {
        &self.zones
    }

    /// Hand the effect to the lighting thread. Reports the thread's last error, if any.
    fn set_light(&mut self, i: usize, e: &Effect) -> Result<(), HwError> {
        let (lock, cv) = &*self.shared;
        let mut s = lock.lock().unwrap_or_else(|e| e.into_inner());
        s.fx[self.channel[i]] = Some(*e);
        cv.notify_all();
        match &s.error {
            Some(err) => Err(HwError(err.clone())),
            None => Ok(()),
        }
    }

    fn save_lights(&mut self) -> Result<(), HwError> {
        Err("the Corsair hub can't store lighting; it shows fan-lite's colours while fan-lite runs".into())
    }
}

impl Drop for CommanderCore {
    /// Stop the lighting thread, which hands the hub back to its own lighting.
    fn drop(&mut self) {
        let (lock, cv) = &*self.shared;
        lock.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        cv.notify_all();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
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

    fn fx(kind: Kind) -> Effect {
        Effect {
            kind,
            colors: [Rgb(1, 2, 3), Rgb(9, 9, 9)],
            ..Default::default()
        }
    }

    #[test]
    fn frame_layout() {
        let mut effects = [None; CHANNELS];
        effects[2] = Some(fx(Kind::Static));
        let leds = [10, 16, 16, 0, 0, 0, 0];
        let f = render(&leds, &effects, 0.0);
        assert_eq!(f.len(), (10 + PORTS * PORT_SLOTS) * 3);
        // Port 2 starts after the strip's 10 LEDs and port 1's 34-LED group.
        let o = (10 + PORT_SLOTS) * 3;
        assert_eq!(&f[o..o + 3], &[1, 2, 3]);
        assert_eq!(&f[o + 33 * 3..o + 34 * 3], &[1, 2, 3]);
        assert!(f[..o].iter().all(|&b| b == 0) && f[o + 34 * 3..].iter().all(|&b| b == 0));
    }

    #[test]
    fn effects_animate() {
        // Breathing: dark at the start of a breath, full halfway, second colour next breath.
        let b = fx(Kind::Breathing);
        let p = period(&b);
        assert_eq!(led_color(&b, 1, 0, 16, 0.0), Rgb::BLACK);
        assert_eq!(led_color(&b, 1, 0, 16, p / 2.0), Rgb(1, 2, 3));
        assert_eq!(led_color(&b, 1, 0, 16, p * 1.5), Rgb(9, 9, 9));
        // Rainbow spreads the hues over a fan's LEDs; colour cycle doesn't.
        let r = fx(Kind::Rainbow);
        assert_ne!(led_color(&r, 1, 0, 16, 0.0), led_color(&r, 1, 8, 16, 0.0));
        let c = fx(Kind::ColorCycle);
        assert_eq!(led_color(&c, 1, 0, 16, 1.0), led_color(&c, 1, 8, 16, 1.0));
        assert_ne!(led_color(&c, 1, 0, 16, 0.0), led_color(&c, 1, 0, 16, 1.0));
        // Wave differs between channels at the same LED.
        let w = fx(Kind::Wave);
        assert_ne!(led_color(&w, 1, 0, 16, 0.0), led_color(&w, 2, 0, 16, 0.0));
        // Brightness applies to every effect.
        let dim = Effect {
            brightness: 0,
            ..fx(Kind::Rainbow)
        };
        assert_eq!(led_color(&dim, 1, 3, 16, 2.0), Rgb::BLACK);
        assert!(!animated(&fx(Kind::Static)) && animated(&fx(Kind::Wave)));
    }

    #[test]
    fn colour_write_packets() {
        let data = render(&[0; CHANNELS], &[Some(fx(Kind::Static)); CHANNELS], 0.0);
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

    /// Through the driver: 10 s rainbow, 10 s wave, 10 s breathing, then the hub goes back to
    /// its own lighting. `cargo test --lib corsair_driver_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn corsair_driver_live() {
        let mut c = CommanderCore::detect().unwrap().expect("no hub");
        for kind in [Kind::Rainbow, Kind::Wave, Kind::Breathing] {
            let e = Effect {
                kind,
                ..Default::default()
            };
            for i in 0..c.lights().len() {
                let _ = c.set_light(i, &e);
            }
            std::thread::sleep(Duration::from_secs(10));
            println!("{kind:?}: {:?}", c.set_light(0, &e));
        }
        drop(c);
        println!("back to the hub's own lighting");
    }
}
