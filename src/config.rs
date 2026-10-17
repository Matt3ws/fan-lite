//! Settings and their tiny line-based file format (no serde).
//!
//! ```text
//! interval_ms 1000
//! critical_c 90
//! unwatch nct/src3
//! hide nct/pwm3
//! label nct/pwm2 "CPU fan"
//! curve "CPU" sensor=amd/tctl hyst=3 points=40:25,60:40,75:70,85:100
//! fan nct/pwm2 mode=curve:"CPU" min=25 up=50 down=10
//! light sync kind=static c1=#f2c94c c2=#1e3cff speed=normal bri=100
//! light zone aura/argb1 kind=wave c1=#f2c94c c2=#1e3cff speed=slow bri=80
//! light leave gpu/0
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::rgb::Effect;

#[derive(Clone, Debug, PartialEq)]
pub struct Curve {
    pub name: String,
    pub sensor: String,
    /// (°C, %) sorted by temperature, non-decreasing in speed.
    pub points: Vec<(f32, f32)>,
    /// °C the temperature must fall before the fan slows down.
    pub hysteresis: f32,
    /// Time constant (seconds) of an exponential average applied to the sensor before the
    /// curve, so short temperature spikes don't make fans surge. 0 = off.
    pub smooth: f32,
}

/// Ready-made curves tuned for a Ryzen 7000 X3D's Tctl (idle ~40-55 °C, gaming ~65-85 °C,
/// Tjmax 89 °C).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    Quiet,
    Balanced,
    Performance,
    /// For an AIO pump: steady and fairly high; pumps should not run slowly.
    AioPump,
}

impl Preset {
    pub const ALL: [Preset; 4] = [Preset::Quiet, Preset::Balanced, Preset::Performance, Preset::AioPump];

    pub fn name(self) -> &'static str {
        match self {
            Preset::Quiet => "Quiet",
            Preset::Balanced => "Balanced",
            Preset::Performance => "Performance",
            Preset::AioPump => "AIO pump",
        }
    }

    /// (points, hysteresis °C, smoothing s). `liquid` selects the table for an AIO's coolant
    /// temperature (idle ~28-32 °C, gaming ~35-42 °C), which moves slowly and in a narrow band.
    fn spec(self, liquid: bool) -> (&'static [(f32, f32)], f32, f32) {
        match (self, liquid) {
            (Preset::Quiet, false) => (
                &[(45.0, 20.0), (60.0, 30.0), (70.0, 45.0), (78.0, 70.0), (85.0, 100.0)],
                4.0,
                5.0,
            ),
            (Preset::Balanced, false) => (
                &[(40.0, 25.0), (55.0, 35.0), (65.0, 50.0), (75.0, 75.0), (82.0, 100.0)],
                3.0,
                4.0,
            ),
            (Preset::Performance, false) => (
                &[(40.0, 35.0), (50.0, 45.0), (60.0, 60.0), (70.0, 80.0), (78.0, 100.0)],
                2.0,
                2.0,
            ),
            (Preset::AioPump, false) => (&[(40.0, 60.0), (65.0, 70.0), (75.0, 85.0), (82.0, 100.0)], 3.0, 5.0),
            (Preset::Quiet, true) => (
                &[(30.0, 20.0), (34.0, 30.0), (37.0, 45.0), (40.0, 70.0), (44.0, 100.0)],
                1.0,
                5.0,
            ),
            (Preset::Balanced, true) => (
                &[(28.0, 25.0), (32.0, 35.0), (35.0, 50.0), (38.0, 75.0), (42.0, 100.0)],
                1.0,
                4.0,
            ),
            (Preset::Performance, true) => (
                &[(28.0, 35.0), (31.0, 45.0), (34.0, 60.0), (37.0, 80.0), (40.0, 100.0)],
                0.5,
                2.0,
            ),
            (Preset::AioPump, true) => (&[(28.0, 50.0), (33.0, 60.0), (38.0, 80.0), (43.0, 100.0)], 1.0, 5.0),
        }
    }
}

/// Sensor id of an AIO's coolant temperature; presets on it use the liquid tables.
pub const LIQUID_SENSOR: &str = "kraken/liquid";

impl Curve {
    pub fn preset(preset: Preset, name: String, sensor: String) -> Self {
        let (points, hysteresis, smooth) = preset.spec(sensor == LIQUID_SENSOR);
        let mut c = Curve {
            name,
            sensor,
            points: points.to_vec(),
            hysteresis,
            smooth,
        };
        c.normalize();
        c
    }

    pub fn new(name: String, sensor: String) -> Self {
        let mut c = Curve {
            name,
            sensor,
            points: vec![(30.0, 25.0), (50.0, 35.0), (70.0, 65.0), (85.0, 100.0)],
            hysteresis: 3.0,
            smooth: 0.0,
        };
        c.normalize();
        c
    }

    /// Enforce the invariants the controller relies on: at least two points, sorted by
    /// temperature, speeds clamped to 0..=100 and never decreasing as temperature rises.
    pub fn normalize(&mut self) {
        for p in &mut self.points {
            p.0 = if p.0.is_finite() { p.0.clamp(0.0, 120.0) } else { 0.0 };
            p.1 = if p.1.is_finite() { p.1.clamp(0.0, 100.0) } else { 100.0 };
        }
        self.points.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut max = 0.0f32;
        for p in &mut self.points {
            max = max.max(p.1);
            p.1 = max;
        }
        while self.points.len() < 2 {
            let last = self.points.last().copied().unwrap_or((40.0, 50.0));
            self.points.push(((last.0 + 20.0).min(120.0), 100.0));
        }
        self.hysteresis = if self.hysteresis.is_finite() {
            self.hysteresis.clamp(0.0, 15.0)
        } else {
            3.0
        };
        self.smooth = if self.smooth.is_finite() {
            self.smooth.clamp(0.0, SMOOTH_MAX)
        } else {
            0.0
        };
    }

    /// Piecewise-linear output, flat beyond the first and last points.
    pub fn eval(&self, t: f32) -> f32 {
        let pts = &self.points;
        let (first, last) = (pts[0], pts[pts.len() - 1]);
        if t <= first.0 {
            return first.1;
        }
        if t >= last.0 {
            return last.1;
        }
        for w in pts.windows(2) {
            let (a, b) = (w[0], w[1]);
            if t <= b.0 {
                let span = b.0 - a.0;
                return if span <= f32::EPSILON {
                    b.1
                } else {
                    a.1 + (b.1 - a.1) * (t - a.0) / span
                };
            }
        }
        last.1
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    /// Firmware/driver controls the fan; we never write to it.
    Auto,
    Manual(f32),
    /// By curve name.
    Curve(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct FanSettings {
    pub id: String,
    pub mode: Mode,
    /// Lowest duty we will ever command (except emergency = 100). 0 allows the fan to stop.
    pub min_pct: f32,
    /// Max change per second when speeding up / slowing down.
    pub ramp_up: f32,
    pub ramp_down: f32,
}

impl FanSettings {
    pub fn new(id: String) -> Self {
        FanSettings {
            id,
            mode: Mode::Auto,
            min_pct: 20.0,
            ramp_up: 50.0,
            ramp_down: 10.0,
        }
    }

    pub fn normalize(&mut self) {
        let c = |v: f32, lo: f32, hi: f32, d: f32| if v.is_finite() { v.clamp(lo, hi) } else { d };
        self.min_pct = c(self.min_pct, 0.0, 100.0, 20.0);
        self.ramp_up = c(self.ramp_up, 1.0, 100.0, 50.0);
        self.ramp_down = c(self.ramp_down, 1.0, 100.0, 10.0);
        if let Mode::Manual(p) = &mut self.mode {
            *p = c(*p, 0.0, 100.0, 100.0);
        }
    }
}

/// What a lighting zone shows.
#[derive(Clone, Debug, PartialEq)]
pub enum ZoneLight {
    /// Its own effect instead of the synced one.
    Own(Effect),
    /// fan-lite never writes to it (another program or the firmware keeps it).
    Leave,
}

/// RGB lighting. With `sync` unset and no zone settings, fan-lite never touches any lights.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Lighting {
    /// One effect for every zone that has no setting of its own; `None` = leave them alone.
    pub sync: Option<Effect>,
    /// Per zone id, overriding `sync`.
    pub zones: BTreeMap<String, ZoneLight>,
}

impl Lighting {
    /// The effect zone `id` should show, or `None` to leave it alone.
    pub fn wanted(&self, id: &str) -> Option<Effect> {
        match self.zones.get(id) {
            Some(ZoneLight::Own(e)) => Some(*e),
            Some(ZoneLight::Leave) => None,
            None => self.sync,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub interval_ms: u32,
    pub critical_c: f32,
    /// Spinning fan icons in the UI (costs a ~60 fps repaint while visible).
    pub animations: bool,
    /// Look for a newer release on GitHub at start and once a day.
    pub update_check: bool,
    pub unwatched: BTreeSet<String>,
    pub labels: BTreeMap<String, String>,
    /// Explicit fan visibility: `false` = hidden, `true` = always shown. Fans without an entry
    /// follow the automatic rule (empty motherboard headers are hidden).
    pub visibility: BTreeMap<String, bool>,
    pub curves: Vec<Curve>,
    pub fans: Vec<FanSettings>,
    /// Measured (duty %, rpm) pairs per control id, from the Calibrate test.
    pub calibrations: BTreeMap<String, Vec<(f32, f32)>>,
    /// Display order of the fan list (control ids, set by dragging). Fans not listed keep
    /// their detection order after the listed ones.
    pub fan_order: Vec<String>,
    pub lighting: Lighting,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            interval_ms: 1000,
            critical_c: 90.0,
            animations: true,
            update_check: true,
            unwatched: BTreeSet::new(),
            labels: BTreeMap::new(),
            visibility: BTreeMap::new(),
            curves: Vec::new(),
            fans: Vec::new(),
            calibrations: BTreeMap::new(),
            fan_order: Vec::new(),
            lighting: Lighting::default(),
        }
    }
}

pub const INTERVAL_RANGE: std::ops::RangeInclusive<u32> = 250..=5000;
pub const CRITICAL_RANGE: std::ops::RangeInclusive<f32> = 60.0..=110.0;
/// Upper bound for a curve's input smoothing, seconds.
pub const SMOOTH_MAX: f32 = 15.0;

impl Config {
    pub fn fan(&self, id: &str) -> Option<&FanSettings> {
        self.fans.iter().find(|f| f.id == id)
    }

    pub fn fan_mut(&mut self, id: &str) -> &mut FanSettings {
        match self.fans.iter().position(|f| f.id == id) {
            Some(i) => &mut self.fans[i],
            None => {
                self.fans.push(FanSettings::new(id.to_owned()));
                self.fans.last_mut().unwrap()
            }
        }
    }

    pub fn curve(&self, name: &str) -> Option<&Curve> {
        self.curves.iter().find(|c| c.name == name)
    }

    pub fn label<'a>(&'a self, id: &str, default: &'a str) -> &'a str {
        self.labels.get(id).map_or(default, String::as_str)
    }

    pub fn normalize(&mut self) {
        self.interval_ms = self.interval_ms.clamp(*INTERVAL_RANGE.start(), *INTERVAL_RANGE.end());
        self.critical_c = if self.critical_c.is_finite() {
            self.critical_c.clamp(*CRITICAL_RANGE.start(), *CRITICAL_RANGE.end())
        } else {
            90.0
        };
        self.curves.iter_mut().for_each(Curve::normalize);
        self.fans.iter_mut().for_each(FanSettings::normalize);
    }

    // ---- file format ----

    pub fn path() -> Option<PathBuf> {
        std::env::var_os("APPDATA").map(|d| Path::new(&d).join("fan-lite").join("config.txt"))
    }

    /// Parse; invalid lines are skipped and reported.
    pub fn parse(text: &str) -> (Config, Vec<String>) {
        let mut cfg = Config::default();
        let mut errors = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Err(e) = parse_line(&mut cfg, line) {
                errors.push(format!("config line {}: {e}", n + 1));
            }
        }
        cfg.normalize();
        (cfg, errors)
    }

    pub fn serialize(&self) -> String {
        let mut s = String::from("# fan-lite configuration\n");
        s += &format!("interval_ms {}\ncritical_c {}\n", self.interval_ms, self.critical_c);
        s += &format!("animations {}\n", if self.animations { "on" } else { "off" });
        s += &format!("update_check {}\n", if self.update_check { "on" } else { "off" });
        for id in &self.unwatched {
            s += &format!("unwatch {id}\n");
        }
        for (id, &shown) in &self.visibility {
            s += &format!("{} {id}\n", if shown { "show" } else { "hide" });
        }
        for (id, l) in &self.labels {
            s += &format!("label {id} {}\n", quote(l));
        }
        for c in &self.curves {
            let pts: Vec<String> = c.points.iter().map(|(t, p)| format!("{t}:{p}")).collect();
            s += &format!(
                "curve {} sensor={} hyst={} smooth={} points={}\n",
                quote(&c.name),
                c.sensor,
                c.hysteresis,
                c.smooth,
                pts.join(",")
            );
        }
        if !self.fan_order.is_empty() {
            s += &format!("order {}\n", self.fan_order.join(" "));
        }
        for (id, pts) in &self.calibrations {
            let pts: Vec<String> = pts.iter().map(|(d, r)| format!("{d}:{r}")).collect();
            s += &format!("calib {id} {}\n", pts.join(","));
        }
        for f in &self.fans {
            let mode = match &f.mode {
                Mode::Auto => "auto".to_owned(),
                Mode::Manual(p) => format!("manual:{p}"),
                Mode::Curve(n) => format!("curve:{}", quote(n)),
            };
            s += &format!(
                "fan {} mode={mode} min={} up={} down={}\n",
                f.id, f.min_pct, f.ramp_up, f.ramp_down
            );
        }
        if let Some(e) = &self.lighting.sync {
            s += &format!("light sync {}\n", e.to_kv());
        }
        for (id, z) in &self.lighting.zones {
            match z {
                ZoneLight::Own(e) => s += &format!("light zone {id} {}\n", e.to_kv()),
                ZoneLight::Leave => s += &format!("light leave {id}\n"),
            }
        }
        s
    }

    pub fn load() -> (Config, Vec<String>) {
        let Some(path) = Self::path() else {
            return (
                Config::default(),
                vec!["APPDATA not set; settings will not be saved".into()],
            );
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), Vec::new()),
            Err(e) => (Config::default(), vec![format!("cannot read {}: {e}", path.display())]),
        }
    }

    /// Write to a temp file and rename over the old one, so a crash never leaves a torn file.
    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path().ok_or_else(|| std::io::Error::other("APPDATA not set"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, self.serialize())?;
        std::fs::rename(&tmp, &path)
    }
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Split a line into tokens; `"..."` is one token (with `\"` and `\\` escapes),
/// and may appear after `key=`.
fn tokenize(line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars();
    let mut in_tok = false;
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_tok = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => cur.push(chars.next().ok_or("dangling escape")?),
                        Some(c) => cur.push(c),
                        None => return Err("unterminated quote".into()),
                    }
                }
            }
            c if c.is_whitespace() => {
                if in_tok {
                    out.push(std::mem::take(&mut cur));
                    in_tok = false;
                }
            }
            c => {
                in_tok = true;
                cur.push(c);
            }
        }
    }
    if in_tok {
        out.push(cur);
    }
    Ok(out)
}

fn num(v: &str) -> Result<f32, String> {
    v.parse::<f32>()
        .ok()
        .filter(|x| x.is_finite())
        .ok_or_else(|| format!("bad number '{v}'"))
}

fn parse_line(cfg: &mut Config, line: &str) -> Result<(), String> {
    let toks = tokenize(line)?;
    let (key, args) = toks.split_first().ok_or("empty")?;
    let one = || args.first().cloned().ok_or_else(|| format!("{key}: missing value"));
    match key.as_str() {
        "interval_ms" => cfg.interval_ms = one()?.parse().map_err(|_| "bad interval")?,
        "critical_c" => cfg.critical_c = num(&one()?)?,
        "animations" => cfg.animations = matches!(one()?.as_str(), "on" | "true" | "1"),
        "update_check" => cfg.update_check = matches!(one()?.as_str(), "on" | "true" | "1"),
        "unwatch" => {
            cfg.unwatched.insert(one()?);
        }
        "hide" => {
            cfg.visibility.insert(one()?, false);
        }
        "show" => {
            cfg.visibility.insert(one()?, true);
        }
        "order" => cfg.fan_order = args.to_vec(),
        "calib" => {
            let [id, pts] = args else {
                return Err("calib: expected id and duty:rpm list".into());
            };
            let mut v = Vec::new();
            for p in pts.split(',') {
                let (d, r) = p
                    .split_once(':')
                    .ok_or_else(|| format!("bad calibration point '{p}'"))?;
                v.push((num(d)?, num(r)?));
            }
            cfg.calibrations.insert(id.clone(), v);
        }
        "label" => {
            let [id, l] = args else {
                return Err("label: expected id and text".into());
            };
            cfg.labels.insert(id.clone(), l.clone());
        }
        "curve" => {
            let (name, kv) = args.split_first().ok_or("curve: missing name")?;
            let mut c = Curve {
                name: name.clone(),
                sensor: String::new(),
                points: Vec::new(),
                hysteresis: 3.0,
                smooth: 0.0,
            };
            for a in kv {
                let (k, v) = a
                    .split_once('=')
                    .ok_or_else(|| format!("expected key=value, got '{a}'"))?;
                match k {
                    "sensor" => c.sensor = v.to_owned(),
                    "hyst" => c.hysteresis = num(v)?,
                    "smooth" => c.smooth = num(v)?,
                    "points" => {
                        for p in v.split(',') {
                            let (t, s) = p.split_once(':').ok_or_else(|| format!("bad point '{p}'"))?;
                            c.points.push((num(t)?, num(s)?));
                        }
                    }
                    _ => return Err(format!("unknown curve key '{k}'")),
                }
            }
            if c.sensor.is_empty() {
                return Err("curve: missing sensor".into());
            }
            if cfg.curves.iter().any(|x| x.name == c.name) {
                return Err(format!("duplicate curve '{}'", c.name));
            }
            cfg.curves.push(c);
        }
        "fan" => {
            let (id, kv) = args.split_first().ok_or("fan: missing id")?;
            let mut f = FanSettings::new(id.clone());
            for a in kv {
                let (k, v) = a
                    .split_once('=')
                    .ok_or_else(|| format!("expected key=value, got '{a}'"))?;
                match k {
                    "mode" => {
                        f.mode = match v.split_once(':') {
                            None if v == "auto" => Mode::Auto,
                            Some(("manual", p)) => Mode::Manual(num(p)?),
                            Some(("curve", n)) => Mode::Curve(n.to_owned()),
                            _ => return Err(format!("bad mode '{v}'")),
                        }
                    }
                    "min" => f.min_pct = num(v)?,
                    "up" => f.ramp_up = num(v)?,
                    "down" => f.ramp_down = num(v)?,
                    _ => return Err(format!("unknown fan key '{k}'")),
                }
            }
            cfg.fans.retain(|x| x.id != f.id);
            cfg.fans.push(f);
        }
        "light" => {
            let effect = |kv: &[String]| -> Result<Effect, String> {
                let mut e = Effect::default();
                for a in kv {
                    let (k, v) = a
                        .split_once('=')
                        .ok_or_else(|| format!("expected key=value, got '{a}'"))?;
                    e.set_kv(k, v)?;
                }
                Ok(e)
            };
            match args {
                [what, kv @ ..] if what == "sync" => cfg.lighting.sync = Some(effect(kv)?),
                [what, id, kv @ ..] if what == "zone" => {
                    cfg.lighting.zones.insert(id.clone(), ZoneLight::Own(effect(kv)?));
                }
                [what, id] if what == "leave" => {
                    cfg.lighting.zones.insert(id.clone(), ZoneLight::Leave);
                }
                _ => return Err("light: expected 'sync …', 'zone <id> …' or 'leave <id>'".into()),
            }
        }
        _ => return Err(format!("unknown setting '{key}'")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rgb::{Kind, Rgb, Speed};

    #[test]
    fn curve_eval_interpolates_and_clamps() {
        let c = Curve {
            name: "c".into(),
            sensor: "s".into(),
            points: vec![(40.0, 20.0), (60.0, 40.0), (80.0, 100.0)],
            hysteresis: 0.0,
            smooth: 0.0,
        };
        assert_eq!(c.eval(0.0), 20.0);
        assert_eq!(c.eval(50.0), 30.0);
        assert_eq!(c.eval(70.0), 70.0);
        assert_eq!(c.eval(200.0), 100.0);
    }

    #[test]
    fn normalize_sorts_and_forces_monotonic() {
        let mut c = Curve {
            name: "c".into(),
            sensor: "s".into(),
            points: vec![(70.0, 50.0), (30.0, 80.0), (90.0, 150.0)],
            hysteresis: -2.0,
            smooth: 0.0,
        };
        c.normalize();
        assert_eq!(c.points, vec![(30.0, 80.0), (70.0, 80.0), (90.0, 100.0)]);
        assert_eq!(c.hysteresis, 0.0);

        let mut one = Curve {
            name: "x".into(),
            sensor: "s".into(),
            points: vec![(50.0, 30.0)],
            hysteresis: 1.0,
            smooth: 0.0,
        };
        one.normalize();
        assert_eq!(one.points.len(), 2);
    }

    #[test]
    fn presets_are_valid_and_monotonic() {
        for (p, sensor) in Preset::ALL
            .into_iter()
            .flat_map(|p| [(p, "amd/tctl"), (p, LIQUID_SENSOR)])
        {
            let c = Curve::preset(p, p.name().into(), sensor.into());
            let mut n = c.clone();
            n.normalize();
            assert_eq!(c, n, "{} changes when normalized", p.name());
            assert!(c.points.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 <= w[1].1));
            assert_eq!(c.points.last().unwrap().1, 100.0, "{} must reach 100%", p.name());
            assert!(c.smooth > 0.0 && c.hysteresis > 0.0);
        }
        // The pump preset never asks for less than 60%.
        let pump = Curve::preset(Preset::AioPump, "p".into(), "t".into());
        assert!(pump.eval(0.0) >= 60.0);
        // On liquid the tables sit in the coolant's narrow range, and the pump stays at 50 %+.
        let liq = Curve::preset(Preset::Balanced, "b".into(), LIQUID_SENSOR.into());
        assert!(liq.points.last().unwrap().0 <= 45.0);
        assert!(Curve::preset(Preset::AioPump, "p".into(), LIQUID_SENSOR.into()).eval(0.0) >= 50.0);
    }

    #[test]
    fn round_trip() {
        let mut cfg = Config {
            interval_ms: 750,
            critical_c: 88.0,
            animations: false,
            update_check: false,
            ..Default::default()
        };
        cfg.unwatched.insert("nct/src3".into());
        cfg.labels.insert("nct/pwm2".into(), "CPU \"main\" fan".into());
        cfg.visibility.insert("nct/pwm3".into(), false);
        cfg.visibility.insert("nct/pwm5".into(), true);
        cfg.curves.push(Curve::new("CPU curve".into(), "amd/tctl".into()));
        cfg.curves
            .push(Curve::preset(Preset::Balanced, "Balanced".into(), "amd/tctl".into()));
        cfg.calibrations
            .insert("nct/pwm2".into(), vec![(100.0, 2700.0), (60.0, 2400.0), (20.0, 2150.0)]);
        cfg.fan_order = vec!["kraken/pump".into(), "nct/pwm4".into(), "nct/pwm2".into()];
        let mut f = FanSettings::new("nct/pwm2".into());
        f.mode = Mode::Curve("CPU curve".into());
        f.min_pct = 25.0;
        cfg.fans.push(f);
        let mut g = FanSettings::new("nvml/0/fan0".into());
        g.mode = Mode::Manual(55.5);
        cfg.fans.push(g);
        cfg.lighting.sync = Some(Effect {
            kind: Kind::Breathing,
            colors: [Rgb(255, 0, 64), Rgb(0, 0, 0)],
            speed: Speed::Slow,
            brightness: 70,
        });
        cfg.lighting.zones.insert(
            "aura/argb1".into(),
            ZoneLight::Own(Effect {
                kind: Kind::Wave,
                ..Default::default()
            }),
        );
        cfg.lighting.zones.insert("gpu/0".into(), ZoneLight::Leave);

        let (back, errs) = Config::parse(&cfg.serialize());
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(back, cfg);
    }

    #[test]
    fn lighting_defaults_to_hands_off() {
        let cfg = Config::default();
        assert_eq!(
            cfg.lighting.wanted("aura/argb1"),
            None,
            "never touch lights unless asked"
        );
        let (cfg, errs) = Config::parse(
            "light sync kind=static c1=#ff0000\nlight leave gpu/0\nlight zone ram/0 kind=off\nlight disco\nlight sync kind=strobe\n",
        );
        assert_eq!(errs.len(), 2, "{errs:?}");
        let red = cfg.lighting.wanted("aura/argb1").unwrap();
        assert_eq!((red.kind, red.colors[0]), (Kind::Static, Rgb(255, 0, 0)));
        assert_eq!(cfg.lighting.wanted("gpu/0"), None);
        assert_eq!(cfg.lighting.wanted("ram/0").unwrap().kind, Kind::Off);
    }

    #[test]
    fn garbage_lines_are_skipped() {
        let text = "interval_ms 2000\nbogus line\nfan x mode=warp\ncurve \"a\" points=1:2\ncritical_c 500\nfan y mode=manual:250\n";
        let (cfg, errs) = Config::parse(text);
        assert_eq!(cfg.interval_ms, 2000);
        assert_eq!(errs.len(), 3);
        assert_eq!(cfg.critical_c, 110.0); // clamped
        assert_eq!(cfg.fan("y").unwrap().mode, Mode::Manual(100.0));
    }
}
