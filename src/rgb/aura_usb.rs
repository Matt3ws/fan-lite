//! ASUS Aura mainboard lighting controller over USB HID (AULA3 "AURA LED Controller",
//! VID 0B05): the board's own LEDs plus its 12 V RGB and 5 V ARGB headers.
//!
//! Protocol as documented by the OpenRGB wiki ("ASUS Aura USB") and inlart's write-up of the
//! X570 controller: 65-byte output reports with report ID `EC`.
//! - `EC 82` → `EC 02 <firmware string>`
//! - `EC B0` → `EC 30 ?? ?? <60-byte config table>`; table[0x02] = ARGB headers,
//!   table[0x1B] = mainboard LEDs (onboard + RGB headers)
//! - `EC 35 <channel> 00 00 <mode>`: effect per channel (0 = mainboard, n = ARGB header n)
//! - `EC 36 <mask hi> <mask lo> 00 <rgb…>`: effect colours; one per mainboard LED, then one per
//!   ARGB header, placed at `5 + 3 * offset` with the mask bits set for the slots sent
//! - `EC 3F 55`: store the current effects in flash
//!
//! The controller runs the effects itself. Effects have no speed or brightness setting, so
//! brightness scales the colour and speed is ignored.

use std::time::{Duration, Instant};

use crate::hw::hid::{self, Hid};
use crate::hw::win::process_running;
use crate::hw::{Backend, Channel, ControlChannel, HwError, LightZone};
use crate::rgb::{Effect, Kind, Rgb};

const VID: &str = "vid_0b05";
/// Mainboard controllers that speak this protocol (lower-case PID in the device path).
const PIDS: [&str; 4] = ["pid_18f3", "pid_1939", "pid_19af", "pid_1aa6"];
const REPORT: usize = 65;
const ID: u8 = 0xEC;
const READ_TIMEOUT_MS: u32 = 200;
const MAX_READS: usize = 8;
/// Armoury Crate's lighting service drives this controller; never write while it runs.
const OWNER: &[&str] = &["lightingservice"];
const OWNER_RECHECK: Duration = Duration::from_secs(5);

/// What the config table says is connected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub argb_headers: u8,
    /// Onboard LEDs plus 12 V RGB headers, all on effect channel 0.
    pub board_leds: u8,
}

/// Decode a config-table reply (`EC 30 …`).
pub fn parse_config(r: &[u8]) -> Option<Layout> {
    if r.len() < 4 + 0x1C || r[0] != ID || r[1] != 0x30 {
        return None;
    }
    let t = &r[4..];
    let l = Layout {
        argb_headers: t[0x02],
        board_leds: t[0x1B],
    };
    // Sanity: 5 V headers and 16 colour slots (two mask bytes) are all the protocol has room for.
    (l.argb_headers <= 8 && l.board_leds as usize + l.argb_headers as usize <= 16).then_some(l)
}

pub fn parse_firmware(r: &[u8]) -> Option<String> {
    if r.len() < 3 || r[0] != ID || r[1] != 0x02 {
        return None;
    }
    let s: String = r[2..]
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| b as char)
        .filter(|c| c.is_ascii_graphic())
        .collect();
    (!s.is_empty()).then_some(s)
}

/// Aura's mode number for an effect kind (only the kinds zones advertise).
fn mode(kind: Kind) -> u8 {
    match kind {
        Kind::Off => 0x00,
        Kind::Static => 0x01,
        Kind::Breathing => 0x02,
        Kind::ColorCycle => 0x04,
        Kind::Rainbow | Kind::Wave => 0x05,
    }
}

const KINDS: [Kind; 5] = [
    Kind::Off,
    Kind::Static,
    Kind::Breathing,
    Kind::ColorCycle,
    Kind::Rainbow,
];

pub fn effect_cmd(channel: u8, kind: Kind) -> [u8; 6] {
    [ID, 0x35, channel, 0x00, 0x00, mode(kind)]
}

/// Colours for colour slots `start..start + colors.len()`.
pub fn color_cmd(start: usize, colors: &[Rgb]) -> Vec<u8> {
    let mask: u16 = (((1u32 << colors.len()) - 1) << start) as u16;
    let mut b = vec![0u8; 5 + 3 * (start + colors.len())];
    b[..5].copy_from_slice(&[ID, 0x36, (mask >> 8) as u8, mask as u8, 0x00]);
    for (k, c) in colors.iter().enumerate() {
        let o = 5 + 3 * (start + k);
        b[o..o + 3].copy_from_slice(&[c.0, c.1, c.2]);
    }
    b
}

pub const SAVE_CMD: [u8; 3] = [ID, 0x3F, 0x55];

pub struct AuraUsb {
    path: Vec<u16>,
    hid: Option<Hid>,
    layout: Layout,
    name: &'static str,
    zones: Vec<LightZone>,
    owner_checked: Option<Instant>,
    owner: Option<String>,
}

impl AuraUsb {
    /// `Ok(None)` if there is no Aura controller.
    pub fn detect() -> Result<Option<Self>, HwError> {
        let Some(dev) = hid::find(VID, None)
            .into_iter()
            .find(|d| PIDS.iter().any(|p| d.lower.contains(p)))
        else {
            return Ok(None);
        };
        // Reading the layout is safe even while Armoury Crate runs: every open handle gets its
        // own copy of the replies. Only writes are held back.
        let hid = Hid::open(&dev.path, REPORT, "ASUS Aura controller")?;
        let fw = hid
            .request(&[ID, 0x82], READ_TIMEOUT_MS, MAX_READS, parse_firmware)
            .unwrap_or_else(|_| "unknown firmware".into());
        let layout = hid.request(&[ID, 0xB0], READ_TIMEOUT_MS, MAX_READS, parse_config)?;
        let mut zones = Vec::new();
        if layout.board_leds > 0 {
            zones.push(LightZone {
                id: "aura/board".into(),
                label: "Motherboard".into(),
                leds: layout.board_leds as u16,
                kinds: KINDS.to_vec(),
            });
        }
        for n in 1..=layout.argb_headers {
            zones.push(LightZone {
                id: format!("aura/argb{n}"),
                label: format!("ARGB header {n}"),
                leds: 0,
                kinds: KINDS.to_vec(),
            });
        }
        let name: &'static str = Box::leak(format!("ASUS Aura ({fw})").into_boxed_str());
        Ok(Some(AuraUsb {
            path: dev.path,
            hid: Some(hid),
            layout,
            name,
            zones,
            owner_checked: None,
            owner: None,
        }))
    }

    /// The open device, unless Armoury Crate's lighting service owns it right now.
    fn device(&mut self) -> Result<&Hid, HwError> {
        if self.owner_checked.is_none_or(|t| t.elapsed() >= OWNER_RECHECK) {
            self.owner_checked = Some(Instant::now());
            self.owner = process_running(OWNER);
        }
        if let Some(o) = &self.owner {
            return Err(HwError(format!(
                "Armoury Crate ({o}) controls the board's lighting - turn it off to use fan-lite"
            )));
        }
        if self.hid.is_none() {
            self.hid = Some(Hid::open(&self.path, REPORT, "ASUS Aura controller")?);
        }
        Ok(self.hid.as_ref().unwrap())
    }

    fn send(&mut self, cmds: &[&[u8]]) -> Result<(), HwError> {
        let r = self.device().and_then(|h| cmds.iter().try_for_each(|c| h.write(c)));
        if r.is_err() {
            self.hid = None; // re-open next time
        }
        r
    }
}

impl Backend for AuraUsb {
    fn name(&self) -> &'static str {
        self.name
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

    fn set_light(&mut self, i: usize, e: &Effect) -> Result<(), HwError> {
        let z = &self.zones[i];
        let board = z.id == "aura/board";
        // Effect channel and colour slots: the mainboard's LEDs first, then one per ARGB header.
        let (channel, start, count) = if board {
            (0u8, 0usize, self.layout.board_leds as usize)
        } else {
            let n: u8 = z.id["aura/argb".len()..].parse().map_err(|_| "bad zone")?;
            (n, self.layout.board_leds as usize + n as usize - 1, 1)
        };
        let effect = effect_cmd(channel, e.kind);
        if e.kind.colors() > 0 {
            let colors = vec![e.colors[0].scaled(e.brightness); count];
            let color = color_cmd(start, &colors);
            self.send(&[&effect, &color])
        } else {
            self.send(&[&effect])
        }
    }

    fn save_lights(&mut self) -> Result<(), HwError> {
        self.send(&[&SAVE_CMD])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_table() {
        let mut r = vec![0u8; 65];
        r[..2].copy_from_slice(&[0xEC, 0x30]);
        r[4 + 0x02] = 3; // ARGB headers
        r[4 + 0x1B] = 5; // mainboard LEDs
        assert_eq!(
            parse_config(&r),
            Some(Layout {
                argb_headers: 3,
                board_leds: 5
            })
        );
        r[4 + 0x1B] = 15; // 15 + 3 slots don't fit the 16-bit mask: not a table we understand
        assert_eq!(parse_config(&r), None);
        r[1] = 0x31;
        assert_eq!(parse_config(&r), None);
    }

    #[test]
    fn firmware_string() {
        let mut r = vec![0u8; 65];
        r[..2].copy_from_slice(&[0xEC, 0x02]);
        r[2..17].copy_from_slice(b"AULA3-AR32-0207");
        assert_eq!(parse_firmware(&r).as_deref(), Some("AULA3-AR32-0207"));
        assert_eq!(parse_firmware(&[0xEC, 0x02, 0]), None);
    }

    #[test]
    fn commands() {
        assert_eq!(effect_cmd(0, Kind::Static), [0xEC, 0x35, 0, 0, 0, 0x01]);
        assert_eq!(effect_cmd(2, Kind::Rainbow), [0xEC, 0x35, 2, 0, 0, 0x05]);
        assert_eq!(effect_cmd(1, Kind::Wave)[5], 0x05, "wave runs as Aura's rainbow");
        // Two board LEDs at slots 0..2.
        let c = color_cmd(0, &[Rgb(1, 2, 3), Rgb(4, 5, 6)]);
        assert_eq!(&c[..11], &[0xEC, 0x36, 0x00, 0b11, 0x00, 1, 2, 3, 4, 5, 6]);
        // ARGB header 2 after 5 board LEDs: slot 6.
        let c = color_cmd(6, &[Rgb(9, 8, 7)]);
        assert_eq!(&c[..5], &[0xEC, 0x36, 0x00, 0b0100_0000, 0x00]);
        assert_eq!(&c[5 + 18..5 + 21], &[9, 8, 7]);
        assert!(c[5..5 + 18].iter().all(|&b| b == 0));
        // Slot 9 lands in the high mask byte.
        assert_eq!(&color_cmd(9, &[Rgb::BLACK])[2..4], &[0b10, 0]);
        assert!(color_cmd(15, &[Rgb::BLACK]).len() <= REPORT);
    }
}

/// Read-only check against real hardware: `cargo test --lib aura_live -- --ignored --nocapture`.
#[cfg(test)]
mod live {
    #[test]
    #[ignore]
    fn aura_live() {
        use crate::hw::Backend;
        match super::AuraUsb::detect() {
            Ok(Some(a)) => {
                println!("{} layout {:?}", a.name(), a.layout);
                for z in a.lights() {
                    println!("  {} {} leds={}", z.id, z.label, z.leds);
                }
            }
            Ok(None) => println!("no Aura controller"),
            Err(e) => println!("error: {e}"),
        }
    }

    /// Writes to real hardware (nothing is saved to flash): steps the board's LEDs through
    /// static red, breathing blue and rainbow, 6 s each.
    /// `cargo test --lib aura_write_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn aura_write_live() {
        use crate::hw::Backend;
        use crate::rgb::{Effect, Kind, Rgb};
        let mut a = super::AuraUsb::detect().unwrap().expect("no Aura controller");
        let steps = [
            (Kind::Static, Rgb(255, 0, 0)),
            (Kind::Breathing, Rgb(0, 60, 255)),
            (Kind::Rainbow, Rgb::BLACK),
        ];
        for (kind, c) in steps {
            let e = Effect {
                kind,
                colors: [c, c],
                ..Default::default()
            };
            for i in 0..a.lights().len() {
                let r = a.set_light(i, &e);
                println!("{:?} -> {}: {r:?}", kind, a.lights()[i].id);
            }
            std::thread::sleep(std::time::Duration::from_secs(6));
        }
    }
}
