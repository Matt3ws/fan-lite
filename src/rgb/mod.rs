//! RGB lighting: the effect model every lighting device understands, and the drivers for
//! devices that only do lighting (devices that also cool, like the Kraken, live in `hw`).
//!
//! Effects run on the devices themselves ("hardware effects"): fan-lite sends an effect once
//! when it changes, so lighting costs no CPU and keeps running after fan-lite exits.

pub mod aura_usb;

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const BLACK: Rgb = Rgb(0, 0, 0);

    /// `#rrggbb`.
    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }

    pub fn parse_hex(s: &str) -> Option<Rgb> {
        let s = s.strip_prefix('#')?;
        if s.len() != 6 || !s.is_ascii() {
            return None;
        }
        let c = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
        Some(Rgb(c(0)?, c(2)?, c(4)?))
    }

    /// The colour at `pct` % brightness, for devices without their own brightness setting.
    pub fn scaled(self, pct: u8) -> Rgb {
        let s = |v: u8| ((v as u32 * pct.min(100) as u32 + 50) / 100) as u8;
        Rgb(s(self.0), s(self.1), s(self.2))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Off,
    /// One colour.
    Static,
    /// Fades the first colour in and out (devices that can alternate also use the second).
    Breathing,
    /// The whole zone fades through the colour wheel.
    ColorCycle,
    /// Every LED shows a different hue, slowly turning (on single-colour zones: like ColorCycle).
    Rainbow,
    /// A rainbow that travels along the LED strip / around the fan ring.
    Wave,
}

impl Kind {
    pub const ALL: [Kind; 6] = [
        Kind::Off,
        Kind::Static,
        Kind::Breathing,
        Kind::ColorCycle,
        Kind::Rainbow,
        Kind::Wave,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Off => "Off",
            Kind::Static => "Static",
            Kind::Breathing => "Breathing",
            Kind::ColorCycle => "Color cycle",
            Kind::Rainbow => "Rainbow",
            Kind::Wave => "Wave",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Kind::Off => "off",
            Kind::Static => "static",
            Kind::Breathing => "breathing",
            Kind::ColorCycle => "cycle",
            Kind::Rainbow => "rainbow",
            Kind::Wave => "wave",
        }
    }

    /// How many of the effect's colours it uses.
    pub fn colors(self) -> usize {
        match self {
            Kind::Static => 1,
            Kind::Breathing => 2,
            _ => 0,
        }
    }

    /// Closest effects, best first, for a zone that can't show this one.
    fn substitutes(self) -> &'static [Kind] {
        match self {
            Kind::Off => &[Kind::Off],
            Kind::Static => &[Kind::Static],
            Kind::Breathing => &[Kind::Breathing, Kind::Static],
            Kind::ColorCycle => &[Kind::ColorCycle, Kind::Rainbow, Kind::Wave, Kind::Static],
            Kind::Rainbow => &[Kind::Rainbow, Kind::Wave, Kind::ColorCycle, Kind::Static],
            Kind::Wave => &[Kind::Wave, Kind::Rainbow, Kind::ColorCycle, Kind::Static],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    Slow,
    Normal,
    Fast,
}

impl Speed {
    pub const ALL: [Speed; 3] = [Speed::Slow, Speed::Normal, Speed::Fast];

    pub fn name(self) -> &'static str {
        match self {
            Speed::Slow => "Slow",
            Speed::Normal => "Normal",
            Speed::Fast => "Fast",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Speed::Slow => "slow",
            Speed::Normal => "normal",
            Speed::Fast => "fast",
        }
    }

    /// 0 = slow, 1 = normal, 2 = fast, for drivers that map it onto a device scale.
    pub fn index(self) -> u8 {
        self as u8
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Effect {
    pub kind: Kind,
    pub colors: [Rgb; 2],
    pub speed: Speed,
    /// 0..=100 %.
    pub brightness: u8,
}

impl Default for Effect {
    fn default() -> Self {
        Effect {
            kind: Kind::Static,
            // fan-lite's amber, and a deep blue for two-colour effects.
            colors: [Rgb(0xF2, 0xC9, 0x4C), Rgb(0x1E, 0x3C, 0xFF)],
            speed: Speed::Normal,
            brightness: 100,
        }
    }
}

impl Effect {
    /// The colour a zone mostly shows, for the UI's colour dot.
    pub fn swatch(&self) -> Option<Rgb> {
        match self.kind {
            Kind::Off => Some(Rgb::BLACK),
            Kind::Static | Kind::Breathing => Some(self.colors[0].scaled(self.brightness)),
            _ => None,
        }
    }

    /// What a zone that supports `supported` should show instead of this effect: the effect
    /// itself if it can, else the closest one. `Off` falls back to static black. `None` if
    /// the zone supports nothing usable.
    pub fn fit(&self, supported: &[Kind]) -> Option<Effect> {
        if let Some(&k) = self.kind.substitutes().iter().find(|k| supported.contains(k)) {
            return Some(Effect { kind: k, ..*self });
        }
        if self.kind == Kind::Off && supported.contains(&Kind::Static) {
            return Some(Effect {
                kind: Kind::Static,
                colors: [Rgb::BLACK; 2],
                ..*self
            });
        }
        None
    }

    /// `kind=… c1=#… c2=#… speed=… bri=…`, the config file form.
    pub fn to_kv(&self) -> String {
        format!(
            "kind={} c1={} c2={} speed={} bri={}",
            self.kind.key(),
            self.colors[0].hex(),
            self.colors[1].hex(),
            self.speed.key(),
            self.brightness
        )
    }

    /// Apply one `key=value` from the config file form.
    pub fn set_kv(&mut self, k: &str, v: &str) -> Result<(), String> {
        match k {
            "kind" => {
                self.kind = Kind::ALL
                    .into_iter()
                    .find(|x| x.key() == v)
                    .ok_or_else(|| format!("unknown effect '{v}'"))?
            }
            "c1" | "c2" => {
                let c = Rgb::parse_hex(v).ok_or_else(|| format!("bad colour '{v}'"))?;
                self.colors[usize::from(k == "c2")] = c;
            }
            "speed" => {
                self.speed = Speed::ALL
                    .into_iter()
                    .find(|x| x.key() == v)
                    .ok_or_else(|| format!("unknown speed '{v}'"))?
            }
            "bri" => self.brightness = v.parse::<u8>().map_err(|_| format!("bad brightness '{v}'"))?.min(100),
            _ => return Err(format!("unknown light key '{k}'")),
        }
        Ok(())
    }
}

impl fmt::Display for Effect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind.name())?;
        if self.kind.colors() > 0 {
            write!(f, " {}", self.colors[0].hex())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip_and_scaling() {
        let c = Rgb(0xF2, 0x0A, 0x00);
        assert_eq!(c.hex(), "#f20a00");
        assert_eq!(Rgb::parse_hex("#f20a00"), Some(c));
        assert_eq!(Rgb::parse_hex("f20a00"), None);
        assert_eq!(Rgb::parse_hex("#f20a0"), None);
        assert_eq!(Rgb::parse_hex("#zz0000"), None);
        assert_eq!(Rgb(200, 100, 0).scaled(50), Rgb(100, 50, 0));
        assert_eq!(Rgb(255, 255, 255).scaled(100), Rgb(255, 255, 255));
        assert_eq!(Rgb(255, 255, 255).scaled(0), Rgb::BLACK);
    }

    #[test]
    fn fit_picks_the_closest_supported_effect() {
        let wave = Effect {
            kind: Kind::Wave,
            ..Default::default()
        };
        let all = Kind::ALL;
        assert_eq!(wave.fit(&all).unwrap().kind, Kind::Wave);
        assert_eq!(wave.fit(&[Kind::Static, Kind::Rainbow]).unwrap().kind, Kind::Rainbow);
        assert_eq!(
            wave.fit(&[Kind::Static, Kind::ColorCycle]).unwrap().kind,
            Kind::ColorCycle
        );
        assert_eq!(wave.fit(&[Kind::Static]).unwrap().kind, Kind::Static);
        assert_eq!(wave.fit(&[]), None);
        let breathing = Effect {
            kind: Kind::Breathing,
            ..Default::default()
        };
        assert_eq!(breathing.fit(&[Kind::Static, Kind::Wave]).unwrap().kind, Kind::Static);
        // Off without an "off" mode is static black, keeping speed/brightness.
        let off = Effect {
            kind: Kind::Off,
            ..Default::default()
        };
        let f = off.fit(&[Kind::Static]).unwrap();
        assert_eq!((f.kind, f.colors), (Kind::Static, [Rgb::BLACK; 2]));
        assert_eq!(off.fit(&[Kind::Off, Kind::Static]).unwrap().kind, Kind::Off);
    }

    #[test]
    fn kv_round_trip() {
        let e = Effect {
            kind: Kind::Breathing,
            colors: [Rgb(1, 2, 3), Rgb(250, 251, 252)],
            speed: Speed::Fast,
            brightness: 40,
        };
        let mut back = Effect::default();
        for kv in e.to_kv().split(' ') {
            let (k, v) = kv.split_once('=').unwrap();
            back.set_kv(k, v).unwrap();
        }
        assert_eq!(back, e);
        assert!(back.set_kv("kind", "disco").is_err());
        assert!(back.set_kv("c1", "red").is_err());
        back.set_kv("bri", "250").unwrap();
        assert_eq!(back.brightness, 100, "clamped");
        assert!(back.set_kv("bri", "-1").is_err());
    }
}
