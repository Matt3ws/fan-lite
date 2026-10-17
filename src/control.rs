//! Pure control and safety logic: no hardware, no threads, fully unit-tested.

use crate::config::{Config, FanSettings, Mode};

/// Temperatures outside this range are treated as sensor faults.
pub const PLAUSIBLE_C: std::ops::RangeInclusive<f32> = -40.0..=125.0;
/// Emergency ends once every watched sensor drops this far below the critical temperature.
pub const EMERGENCY_HYST_C: f32 = 5.0;
/// Consecutive missed reads before a sensor is considered failed.
pub const MAX_MISSES: u32 = 3;
pub const STALL_MIN_DUTY: f32 = 30.0;
pub const STALL_SECS: f32 = 5.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Auto,
    Manual,
    Curve,
    /// A watched sensor is at or above the critical temperature.
    Emergency,
    /// The curve's sensor is missing or failing.
    SensorFault,
    /// Writes failed repeatedly; handed back to firmware.
    Fault,
    /// Being swept through fixed duties by the Calibrate test.
    Calibrating,
}

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Reason::Auto => "auto",
            Reason::Manual => "manual",
            Reason::Curve => "curve",
            Reason::Emergency => "EMERGENCY",
            Reason::SensorFault => "sensor fault",
            Reason::Fault => "fault",
            Reason::Calibrating => "calibrating",
        }
    }
}

/// What the controller wants for one fan this tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Target {
    /// `None` = release to firmware/driver control.
    pub duty: Option<f32>,
    pub reason: Reason,
    /// Safety overrides skip ramp limiting.
    pub immediate: bool,
}

/// Latch the emergency state with a release band, so it doesn't flap at the threshold.
/// `hottest` is the max over watched, valid sensors.
pub fn emergency(active: bool, hottest: Option<f32>, critical: f32) -> bool {
    match hottest {
        Some(t) if t >= critical => true,
        Some(t) => active && t > critical - EMERGENCY_HYST_C,
        // All watched sensors vanished: hold the current state rather than relaxing.
        None => active,
    }
}

/// Hysteresis on the way down only: a rise is followed immediately, a fall is ignored
/// until it exceeds `h`. Returns the effective temperature to feed the curve.
pub fn hysteresis(prev_effective: Option<f32>, t: f32, h: f32) -> f32 {
    match prev_effective {
        Some(p) if t < p && p - t < h => p,
        _ => t,
    }
}

/// One step of an exponential moving average with time constant `tau` seconds.
/// `tau <= 0` (or no previous value) passes the new reading straight through.
pub fn ema(prev: Option<f32>, t: f32, dt: f32, tau: f32) -> f32 {
    match prev {
        Some(p) if tau > 0.0 => p + (t - p) * (1.0 - (-dt.max(0.0) / tau).exp()),
        _ => t,
    }
}

/// What a calibration sweep says about a fan or pump.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Verdict {
    /// Speed changes meaningfully with duty, so curves will have an effect.
    pub responds: bool,
    /// Still above ~1500 rpm at the lowest duty tested: almost certainly a pump.
    pub likely_pump: bool,
    /// Lowest duty tested at which it still turned.
    pub min_spin_duty: Option<f32>,
    pub rpm_at_min: f32,
    pub rpm_at_max: f32,
}

/// Interpret (duty %, rpm) pairs from a calibration sweep.
pub fn analyze(points: &[(f32, f32)]) -> Option<Verdict> {
    let lo = points.iter().copied().min_by(|a, b| a.0.total_cmp(&b.0))?;
    let hi = points.iter().copied().max_by(|a, b| a.0.total_cmp(&b.0))?;
    let max_rpm = points.iter().map(|p| p.1).fold(0.0f32, f32::max);
    let min_rpm = points.iter().map(|p| p.1).fold(f32::INFINITY, f32::min);
    Some(Verdict {
        responds: max_rpm > 0.0 && (max_rpm - min_rpm) > 0.15 * max_rpm,
        likely_pump: lo.1 > 1500.0,
        min_spin_duty: points.iter().filter(|p| p.1 > 0.0).map(|p| p.0).min_by(f32::total_cmp),
        rpm_at_min: lo.1,
        rpm_at_max: hi.1,
    })
}

/// Limit how fast the duty may change. `prev = None` (just taking over) jumps directly.
pub fn ramp(prev: Option<f32>, target: f32, up_per_s: f32, down_per_s: f32, dt_s: f32) -> f32 {
    let Some(p) = prev else { return target };
    if target > p {
        target.min(p + up_per_s * dt_s)
    } else {
        target.max(p - down_per_s * dt_s)
    }
}

/// Decide the target for one fan, before ramp limiting.
///
/// `curve_temp(name)` returns the curve's effective (post-hysteresis) temperature, or `None`
/// if its sensor is failing. `hw_min` is the hardware's minimum duty.
pub fn target(
    fan: &FanSettings,
    hw_min: f32,
    cfg: &Config,
    emergency: bool,
    curve_temp: impl Fn(&str) -> Option<f32>,
) -> Target {
    let floor = fan.min_pct.max(hw_min).clamp(0.0, 100.0);
    let clamp = |v: f32| v.clamp(floor, 100.0);
    let full = |reason| Target {
        duty: Some(100.0),
        reason,
        immediate: true,
    };
    match &fan.mode {
        Mode::Auto => Target {
            duty: None,
            reason: Reason::Auto,
            immediate: true,
        },
        _ if emergency => full(Reason::Emergency),
        Mode::Manual(p) => Target {
            duty: Some(clamp(*p)),
            reason: Reason::Manual,
            immediate: false,
        },
        Mode::Curve(name) => match cfg.curve(name).zip(curve_temp(name)) {
            Some((c, t)) => Target {
                duty: Some(clamp(c.eval(t))),
                reason: Reason::Curve,
                immediate: false,
            },
            // Missing curve or failing sensor: fail safe.
            None => full(Reason::SensorFault),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Curve;

    fn cfg_with_curve() -> Config {
        let mut cfg = Config::default();
        cfg.curves.push(Curve {
            name: "cpu".into(),
            sensor: "t".into(),
            points: vec![(40.0, 0.0), (80.0, 100.0)],
            hysteresis: 3.0,
            smooth: 0.0,
        });
        cfg
    }

    fn fan(mode: Mode, min: f32) -> FanSettings {
        let mut f = FanSettings::new("f".into());
        f.mode = mode;
        f.min_pct = min;
        f
    }

    #[test]
    fn emergency_band() {
        assert!(!emergency(false, Some(89.9), 90.0));
        assert!(emergency(false, Some(90.0), 90.0));
        assert!(emergency(true, Some(86.0), 90.0)); // still inside band
        assert!(!emergency(true, Some(85.0), 90.0)); // released
        assert!(emergency(true, None, 90.0)); // sensors gone: hold
        assert!(!emergency(false, None, 90.0));
    }

    #[test]
    fn hysteresis_only_on_fall() {
        assert_eq!(hysteresis(None, 50.0, 3.0), 50.0);
        assert_eq!(hysteresis(Some(50.0), 55.0, 3.0), 55.0); // rise: immediate
        assert_eq!(hysteresis(Some(55.0), 53.0, 3.0), 55.0); // small fall: hold
        assert_eq!(hysteresis(Some(55.0), 52.0, 3.0), 52.0); // big fall: follow
    }

    #[test]
    fn ema_smooths_and_converges() {
        assert_eq!(ema(None, 70.0, 1.0, 5.0), 70.0);
        assert_eq!(ema(Some(50.0), 70.0, 1.0, 0.0), 70.0); // smoothing off
        let one = ema(Some(50.0), 70.0, 1.0, 5.0);
        assert!(one > 50.0 && one < 55.0, "a 1 s step moves ~18% of the way: {one}");
        let mut v = 50.0;
        for _ in 0..60 {
            v = ema(Some(v), 70.0, 1.0, 5.0);
        }
        assert!((v - 70.0).abs() < 0.01);
    }

    #[test]
    fn analyze_fan_pump_and_unresponsive() {
        // Normal fan: speed tracks duty, stops below 25%.
        let fan = analyze(&[(100.0, 1800.0), (60.0, 1100.0), (30.0, 500.0), (20.0, 0.0)]).unwrap();
        assert!(fan.responds && !fan.likely_pump);
        assert_eq!(fan.min_spin_duty, Some(30.0));
        // PWM pump: responds a little but stays fast.
        let pump = analyze(&[(100.0, 2800.0), (60.0, 2400.0), (20.0, 2150.0)]).unwrap();
        assert!(pump.responds && pump.likely_pump);
        // Ignores PWM entirely (DC header / 3-pin / fixed-speed pump).
        let fixed = analyze(&[(100.0, 2200.0), (50.0, 2190.0), (20.0, 2205.0)]).unwrap();
        assert!(!fixed.responds);
        assert_eq!(analyze(&[]), None);
    }

    #[test]
    fn ramp_limits() {
        assert_eq!(ramp(None, 80.0, 10.0, 5.0, 1.0), 80.0);
        assert_eq!(ramp(Some(20.0), 80.0, 10.0, 5.0, 1.0), 30.0);
        assert_eq!(ramp(Some(80.0), 20.0, 10.0, 5.0, 0.5), 77.5);
        assert_eq!(ramp(Some(50.0), 52.0, 10.0, 5.0, 1.0), 52.0);
    }

    #[test]
    fn curve_target_respects_floor() {
        let cfg = cfg_with_curve();
        let f = fan(Mode::Curve("cpu".into()), 20.0);
        let t = target(&f, 0.0, &cfg, false, |_| Some(44.0)); // curve says 10%
        assert_eq!(t.duty, Some(20.0));
        assert_eq!(t.reason, Reason::Curve);
        let t = target(&f, 30.0, &cfg, false, |_| Some(60.0)); // curve 50% is above hw min
        assert_eq!(t.duty, Some(50.0));
        let t = target(&f, 30.0, &cfg, false, |_| Some(40.0)); // hw min 30% beats user min 20%
        assert_eq!(t.duty, Some(30.0));
    }

    #[test]
    fn sensor_fault_and_missing_curve_fail_safe() {
        let cfg = cfg_with_curve();
        let t = target(&fan(Mode::Curve("cpu".into()), 0.0), 0.0, &cfg, false, |_| None);
        assert_eq!(
            (t.duty, t.reason, t.immediate),
            (Some(100.0), Reason::SensorFault, true)
        );
        let t = target(&fan(Mode::Curve("gone".into()), 0.0), 0.0, &cfg, false, |_| Some(40.0));
        assert_eq!((t.duty, t.reason), (Some(100.0), Reason::SensorFault));
    }

    #[test]
    fn emergency_overrides_controlled_fans_only() {
        let cfg = cfg_with_curve();
        let t = target(&fan(Mode::Manual(20.0), 0.0), 0.0, &cfg, true, |_| Some(40.0));
        assert_eq!((t.duty, t.reason, t.immediate), (Some(100.0), Reason::Emergency, true));
        let t = target(&fan(Mode::Auto, 0.0), 0.0, &cfg, true, |_| Some(40.0));
        assert_eq!(t.duty, None); // firmware keeps its own control
    }

    #[test]
    fn manual_is_clamped_to_floor() {
        let cfg = Config::default();
        let t = target(&fan(Mode::Manual(5.0), 20.0), 0.0, &cfg, false, |_| None);
        assert_eq!(t.duty, Some(20.0));
        let t = target(&fan(Mode::Manual(0.0), 0.0), 0.0, &cfg, false, |_| None);
        assert_eq!(t.duty, Some(0.0)); // explicit 0 allowed when min is 0
    }
}
