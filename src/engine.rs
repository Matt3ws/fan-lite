//! Background worker: owns the hardware, runs the control loop, publishes snapshots.
//!
//! The thread sleeps in `recv_timeout` until the next tick or a command arrives; there is no
//! other polling. Dropping [`Engine`] (normal exit, shutdown, or panic unwinding) hands every
//! fan we touched back to firmware/driver control.

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::config::{Config, FanSettings};
use crate::control::{self, MAX_MISSES, PLAUSIBLE_C, Reason, STALL_MIN_DUTY, STALL_SECS};
use crate::hw::{Backend, detect_all};
use crate::rgb::{Effect, Kind};

/// Re-send the duty this often even if unchanged, in case firmware or the driver overrode it.
const REASSERT: Duration = Duration::from_secs(10);
/// Smallest duty change worth a hardware write.
const MIN_STEP: f32 = 0.5;
/// After this many consecutive write failures, give the fan back to firmware.
const MAX_WRITE_FAILS: u32 = 3;
/// Config edits (e.g. dragging a slider) trigger a tick at most this often.
const MIN_TICK_GAP: Duration = Duration::from_millis(100);
/// Duties the Calibrate test steps through, fastest first (never below the fan's floor).
const CALIB_STEPS: [f32; 7] = [100.0, 80.0, 60.0, 45.0, 35.0, 25.0, 20.0];
/// Per step: let the speed settle, then average the RPM over the sample window.
const CALIB_SETTLE: Duration = Duration::from_secs(4);
const CALIB_SAMPLE: Duration = Duration::from_secs(2);
/// Abort calibration when a watched sensor gets this close to the critical temperature.
const CALIB_ABORT_MARGIN_C: f32 = 10.0;
/// After a failed lighting write, wait this long before trying that zone again.
const LIGHT_RETRY: Duration = Duration::from_secs(5);

/// A running Calibrate sweep on one control.
struct Calib {
    k: usize,
    steps: Vec<f32>,
    idx: usize,
    step_start: Instant,
    samples: Vec<f32>,
    results: Vec<(f32, f32)>,
}

/// Outcome of the last calibration, for the UI.
#[derive(Clone, Debug, Default)]
pub struct CalibReport {
    /// Increments per finished or aborted run, so the UI can tell a new result.
    pub seq: u64,
    pub id: String,
    /// (duty %, rpm), fastest first. Partial if aborted.
    pub results: Vec<(f32, f32)>,
    /// Why it stopped early, if it did.
    pub aborted: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ChannelInfo {
    pub id: String,
    pub label: String,
    pub backend: &'static str,
}

#[derive(Clone, Debug)]
pub struct ControlInfo {
    pub id: String,
    pub label: String,
    pub backend: &'static str,
    pub hw_min_pct: f32,
    /// Index into `Inventory::fans`.
    pub rpm_fan: Option<usize>,
    /// May be an empty header (see `hw::ControlChannel::optional`).
    pub optional: bool,
}

#[derive(Clone, Debug)]
pub struct LightInfo {
    pub id: String,
    pub label: String,
    pub backend: &'static str,
    pub leds: u16,
    pub kinds: Vec<Kind>,
    /// The device can store its lighting ("Save to device").
    pub saveable: bool,
}

/// Outcome of a "Save to device".
#[derive(Clone, Debug, Default)]
pub struct LightSave {
    /// Increments per save, so the UI can tell a new result.
    pub seq: u64,
    /// Devices that stored their lighting.
    pub saved: Vec<String>,
    pub errors: Vec<String>,
}

/// Static description of the detected hardware.
#[derive(Clone, Debug, Default)]
pub struct Inventory {
    pub temps: Vec<ChannelInfo>,
    pub fans: Vec<ChannelInfo>,
    pub controls: Vec<ControlInfo>,
    pub lights: Vec<LightInfo>,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ControlView {
    /// Duty read back from hardware.
    pub duty: Option<f32>,
    /// Duty we last wrote; `None` when firmware is in control.
    pub commanded: Option<f32>,
    pub reason: Reason,
    pub stalled: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub ready: bool,
    pub inv: Arc<Inventory>,
    pub temps: Vec<Option<f32>>,
    pub fans: Vec<Option<f32>>,
    pub controls: Vec<ControlView>,
    pub emergency: bool,
    /// Effective (post-hysteresis) input temperature per curve name.
    pub curve_temps: Vec<(String, f32)>,
    /// Per fan RPM input: has it reported a non-zero speed since startup?
    pub fan_seen: Vec<bool>,
    /// Increments once per engine tick, so the UI can tell fresh data from a plain repaint.
    pub tick: u64,
    /// Calibration in progress: (control id, current duty, step number from 1, step count).
    pub calibrating: Option<(String, f32, usize, usize)>,
    pub last_calib: Option<CalibReport>,
    pub lights: Vec<LightView>,
    pub light_save: Option<LightSave>,
}

#[derive(Clone, Debug, Default)]
pub struct LightView {
    /// Effect last sent to the zone (after fitting it to what the zone supports).
    pub applied: Option<Effect>,
    pub error: Option<String>,
}

impl Snapshot {
    pub fn temp(&self, id: &str) -> Option<f32> {
        let i = self.inv.temps.iter().position(|t| t.id == id)?;
        self.temps.get(i).copied().flatten()
    }
}

enum Command {
    SetConfig(Config),
    Calibrate(String),
    SaveLights,
    Shutdown,
}

pub struct EngineHandle {
    tx: Sender<Command>,
    shared: Arc<Mutex<Snapshot>>,
    thread: Option<JoinHandle<()>>,
}

impl EngineHandle {
    pub fn spawn(cfg: Config, notify: Box<dyn Fn() + Send>) -> Self {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Snapshot::default()));
        let out = shared.clone();
        let thread = std::thread::Builder::new()
            .name("fan-engine".into())
            .spawn(move || {
                let det = detect_all();
                let mut engine = Engine::new(det.backends, det.diagnostics, cfg);
                engine.publish(&out);
                notify();
                let mut next = Instant::now();
                loop {
                    match rx.recv_timeout(next.saturating_duration_since(Instant::now())) {
                        Ok(Command::SetConfig(c)) => {
                            engine.cfg = c;
                            next = next.min(engine.last_tick + MIN_TICK_GAP);
                        }
                        Ok(Command::Calibrate(id)) => {
                            engine.start_calibration(&id);
                            next = next.min(engine.last_tick + MIN_TICK_GAP);
                        }
                        Ok(Command::SaveLights) => {
                            engine.save_lights();
                            engine.publish(&out);
                            notify();
                        }
                        Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {
                            engine.tick();
                            engine.publish(&out);
                            notify();
                            next = engine.last_tick + Duration::from_millis(engine.cfg.interval_ms as u64);
                        }
                    }
                }
                // `engine` drops here (or during unwinding) and restores firmware control.
            })
            .expect("spawn engine thread");
        EngineHandle {
            tx,
            shared,
            thread: Some(thread),
        }
    }

    pub fn set_config(&self, cfg: Config) {
        let _ = self.tx.send(Command::SetConfig(cfg));
    }

    /// Start the Calibrate sweep on one control (ignored if one is already running).
    pub fn calibrate(&self, id: &str) {
        let _ = self.tx.send(Command::Calibrate(id.to_owned()));
    }

    /// Store the current lighting in every device's flash.
    pub fn save_lights(&self) {
        let _ = self.tx.send(Command::SaveLights);
    }

    pub fn snapshot(&self) -> MutexGuard<'_, Snapshot> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// False if the engine thread died (it restores fans while unwinding).
    pub fn alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[derive(Default)]
struct TempState {
    last: Option<f32>,
    misses: u32,
}

impl TempState {
    fn value(&self) -> Option<f32> {
        if self.misses < MAX_MISSES { self.last } else { None }
    }
}

struct FanState {
    /// Duty last written successfully; `None` = firmware in control.
    applied: Option<f32>,
    last_write: Instant,
    fails: u32,
    /// Set after repeated write failures; cleared when the user changes the fan's settings.
    fault: Option<FanSettings>,
    stall_secs: f32,
    reason: Reason,
    error: Option<String>,
}

#[derive(Default)]
struct LightState {
    applied: Option<Effect>,
    error: Option<String>,
    /// The effect whose write last failed, and the earliest time to try it again (a
    /// different effect is tried at once).
    failed: Option<(Effect, Instant)>,
}

/// Where a flat index lives: (backend, local index).
type Loc = (usize, usize);

pub struct Engine {
    backends: Vec<Box<dyn Backend>>,
    pub cfg: Config,
    inv: Arc<Inventory>,
    temp_loc: Vec<Loc>,
    fan_loc: Vec<Loc>,
    ctl_loc: Vec<Loc>,
    light_loc: Vec<Loc>,
    lights: Vec<LightState>,
    light_save: Option<LightSave>,
    // Per-backend read buffers.
    buf_t: Vec<Vec<Option<f32>>>,
    buf_f: Vec<Vec<Option<f32>>>,
    buf_d: Vec<Vec<Option<f32>>>,
    temps: Vec<TempState>,
    fans: Vec<Option<f32>>,
    duty: Vec<Option<f32>>,
    ctl: Vec<FanState>,
    curve_eff: Vec<(String, f32)>,
    /// Smoothed (EMA) sensor value per curve name, before hysteresis.
    curve_smooth: Vec<(String, f32)>,
    /// Hottest watched temperature this tick.
    hottest: Option<f32>,
    calib: Option<Calib>,
    last_calib: Option<CalibReport>,
    calib_settle: Duration,
    calib_sample: Duration,
    fan_seen: Vec<bool>,
    emergency: bool,
    ready: bool,
    ticks: u64,
    pub last_tick: Instant,
}

impl Engine {
    pub fn new(backends: Vec<Box<dyn Backend>>, diagnostics: Vec<String>, cfg: Config) -> Self {
        let mut inv = Inventory {
            diagnostics,
            ..Default::default()
        };
        let (mut temp_loc, mut fan_loc, mut ctl_loc, mut light_loc) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (b, be) in backends.iter().enumerate() {
            let fan_base = inv.fans.len();
            for (i, c) in be.temps().iter().enumerate() {
                inv.temps.push(ChannelInfo {
                    id: c.id.clone(),
                    label: c.label.clone(),
                    backend: be.name(),
                });
                temp_loc.push((b, i));
            }
            for (i, c) in be.fans().iter().enumerate() {
                inv.fans.push(ChannelInfo {
                    id: c.id.clone(),
                    label: c.label.clone(),
                    backend: be.name(),
                });
                fan_loc.push((b, i));
            }
            for (i, c) in be.controls().iter().enumerate() {
                inv.controls.push(ControlInfo {
                    id: c.id.clone(),
                    label: c.label.clone(),
                    backend: be.name(),
                    hw_min_pct: c.hw_min_pct,
                    rpm_fan: c.rpm_fan.map(|f| fan_base + f),
                    optional: c.optional,
                });
                ctl_loc.push((b, i));
            }
            for (i, z) in be.lights().iter().enumerate() {
                inv.lights.push(LightInfo {
                    id: z.id.clone(),
                    label: z.label.clone(),
                    backend: be.name(),
                    leds: z.leds,
                    kinds: z.kinds.clone(),
                    saveable: be.can_save_lights(),
                });
                light_loc.push((b, i));
            }
        }
        let now = Instant::now();
        Engine {
            buf_t: backends.iter().map(|b| vec![None; b.temps().len()]).collect(),
            buf_f: backends.iter().map(|b| vec![None; b.fans().len()]).collect(),
            buf_d: backends.iter().map(|b| vec![None; b.controls().len()]).collect(),
            temps: (0..inv.temps.len()).map(|_| TempState::default()).collect(),
            fans: vec![None; inv.fans.len()],
            fan_seen: vec![false; inv.fans.len()],
            duty: vec![None; inv.controls.len()],
            ctl: (0..inv.controls.len())
                .map(|_| FanState {
                    applied: None,
                    last_write: now,
                    fails: 0,
                    fault: None,
                    stall_secs: 0.0,
                    reason: Reason::Auto,
                    error: None,
                })
                .collect(),
            lights: (0..inv.lights.len()).map(|_| LightState::default()).collect(),
            light_loc,
            light_save: None,
            backends,
            cfg,
            inv: Arc::new(inv),
            temp_loc,
            fan_loc,
            ctl_loc,
            curve_eff: Vec::new(),
            curve_smooth: Vec::new(),
            hottest: None,
            calib: None,
            last_calib: None,
            calib_settle: CALIB_SETTLE,
            calib_sample: CALIB_SAMPLE,
            emergency: false,
            ready: false,
            ticks: 0,
            last_tick: now - Duration::from_secs(1),
        }
    }

    pub fn tick(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32().clamp(0.0, 5.0);
        self.last_tick = now;
        self.read_all();
        self.update_safety_inputs(dt);
        self.advance_calibration(now);
        for k in 0..self.ctl.len() {
            self.control_one(k, dt, now);
        }
        self.apply_lights(now);
        self.ready = true;
        self.ticks += 1;
    }

    fn read_all(&mut self) {
        for (b, be) in self.backends.iter_mut().enumerate() {
            be.update(&mut self.buf_t[b], &mut self.buf_f[b], &mut self.buf_d[b]);
        }
        for (st, &(b, i)) in self.temps.iter_mut().zip(&self.temp_loc) {
            match self.buf_t[b][i].filter(|v| PLAUSIBLE_C.contains(v)) {
                Some(v) => {
                    *st = TempState {
                        last: Some(v),
                        misses: 0,
                    }
                }
                None => st.misses = st.misses.saturating_add(1),
            }
        }
        for (k, &(b, i)) in self.fan_loc.iter().enumerate() {
            let v = self.buf_f[b][i];
            self.fans[k] = v;
            self.fan_seen[k] |= v.is_some_and(|r| r > 0.0);
        }
        for (v, &(b, i)) in self.duty.iter_mut().zip(&self.ctl_loc) {
            *v = self.buf_d[b][i];
        }
    }

    fn temp_by_id(&self, id: &str) -> Option<f32> {
        let i = self.inv.temps.iter().position(|t| t.id == id)?;
        self.temps[i].value()
    }

    /// Begin the Calibrate sweep on control `id`. Needs an RPM input; one run at a time.
    pub fn start_calibration(&mut self, id: &str) {
        if self.calib.is_some() || self.emergency {
            return;
        }
        let Some(k) = self.inv.controls.iter().position(|c| c.id == id) else {
            return;
        };
        if self.ctl[k].fault.is_some() {
            return;
        }
        let info = &self.inv.controls[k];
        let report = |aborted: &str, seq| CalibReport {
            seq,
            id: id.to_owned(),
            results: Vec::new(),
            aborted: Some(aborted.into()),
        };
        let seq = self.last_calib.as_ref().map_or(0, |r| r.seq) + 1;
        if info.rpm_fan.is_none() {
            self.last_calib = Some(report("this output has no speed sensor to measure", seq));
            return;
        }
        // Never below the fan's floor, the hardware minimum, or 20 % (the test never stops a
        // fan or pump; firmware already runs them at 20 %).
        let min_pct = self
            .cfg
            .fan(id)
            .map_or(FanSettings::new(String::new()).min_pct, |f| f.min_pct);
        let floor = min_pct.max(info.hw_min_pct).max(CALIB_STEPS[CALIB_STEPS.len() - 1]);
        let mut steps: Vec<f32> = CALIB_STEPS.iter().copied().filter(|&d| d >= floor).collect();
        if steps.last().is_none_or(|&d| d > floor) {
            steps.push(floor);
        }
        self.calib = Some(Calib {
            k,
            steps,
            idx: 0,
            step_start: Instant::now(),
            samples: Vec::new(),
            results: Vec::new(),
        });
    }

    fn finish_calibration(&mut self, aborted: Option<String>) {
        let Some(c) = self.calib.take() else { return };
        let seq = self.last_calib.as_ref().map_or(0, |r| r.seq) + 1;
        self.last_calib = Some(CalibReport {
            seq,
            id: self.inv.controls[c.k].id.clone(),
            results: c.results,
            aborted,
        });
    }

    /// Step the sweep: let each duty settle, average the RPM, move on. Aborts on heat.
    fn advance_calibration(&mut self, now: Instant) {
        let Some(c) = &mut self.calib else { return };
        let too_hot = self
            .hottest
            .is_some_and(|t| t >= self.cfg.critical_c - CALIB_ABORT_MARGIN_C);
        if self.emergency || too_hot {
            self.finish_calibration(Some("stopped: a watched sensor got too hot".into()));
            return;
        }
        let elapsed = now.duration_since(c.step_start);
        if elapsed >= self.calib_settle
            && let Some(rpm) = self.inv.controls[c.k].rpm_fan.and_then(|f| self.fans[f])
        {
            c.samples.push(rpm);
        }
        if elapsed >= self.calib_settle + self.calib_sample {
            let rpm = if c.samples.is_empty() {
                0.0
            } else {
                c.samples.iter().sum::<f32>() / c.samples.len() as f32
            };
            c.results.push((c.steps[c.idx], rpm.round()));
            c.idx += 1;
            c.samples.clear();
            c.step_start = now;
            if c.idx == c.steps.len() {
                self.finish_calibration(None);
            }
        }
    }

    fn update_safety_inputs(&mut self, dt: f32) {
        let hottest = self
            .inv
            .temps
            .iter()
            .zip(&self.temps)
            .filter(|(info, _)| !self.cfg.unwatched.contains(&info.id))
            .filter_map(|(_, st)| st.value())
            .reduce(f32::max);
        // Safety uses raw temperatures: smoothing below only feeds the curves.
        self.hottest = hottest;
        self.emergency = control::emergency(self.emergency, hottest, self.cfg.critical_c);

        let mut eff = Vec::with_capacity(self.cfg.curves.len());
        let mut smooth = Vec::with_capacity(self.cfg.curves.len());
        for c in &self.cfg.curves {
            if let Some(t) = self.temp_by_id(&c.sensor) {
                let prev_s = self.curve_smooth.iter().find(|(n, _)| *n == c.name).map(|e| e.1);
                let s = control::ema(prev_s, t, dt, c.smooth);
                smooth.push((c.name.clone(), s));
                let prev = self.curve_eff.iter().find(|(n, _)| *n == c.name).map(|e| e.1);
                eff.push((c.name.clone(), control::hysteresis(prev, s, c.hysteresis)));
            }
        }
        self.curve_eff = eff;
        self.curve_smooth = smooth;
    }

    fn control_one(&mut self, k: usize, dt: f32, now: Instant) {
        let info = &self.inv.controls[k];
        let fan = self
            .cfg
            .fan(&info.id)
            .cloned()
            .unwrap_or_else(|| FanSettings::new(info.id.clone()));
        let (b, i) = self.ctl_loc[k];
        let be = &mut self.backends[b];
        let st = &mut self.ctl[k];

        // A faulted fan stays with firmware until the user changes its settings.
        if let Some(f) = &st.fault {
            if *f == fan {
                st.reason = Reason::Fault;
                return;
            }
            st.fault = None;
            st.fails = 0;
            st.error = None;
        }

        let curve_eff = &self.curve_eff;
        let mut tgt = control::target(&fan, info.hw_min_pct, &self.cfg, self.emergency, |name| {
            curve_eff.iter().find(|(n, _)| n == name).map(|e| e.1)
        });
        // Calibration overrides the normal target (emergency already aborted it this tick).
        let calibrating = self.calib.as_ref().filter(|c| c.k == k);
        if let Some(c) = calibrating {
            tgt = control::Target {
                duty: Some(c.steps[c.idx]),
                reason: Reason::Calibrating,
                immediate: true,
            };
        }
        let is_calib = calibrating.is_some();
        st.reason = tgt.reason;

        match tgt.duty {
            None => {
                if st.applied.is_some() {
                    match be.release(i) {
                        Ok(()) => st.applied = None,
                        Err(e) => st.error = Some(format!("release failed: {e}")),
                    }
                }
            }
            Some(d) => {
                let d = if tgt.immediate {
                    d
                } else {
                    control::ramp(st.applied, d, fan.ramp_up, fan.ramp_down, dt)
                };
                let due = match st.applied {
                    None => true,
                    Some(a) => (d - a).abs() >= MIN_STEP || now.duration_since(st.last_write) >= REASSERT,
                };
                if due {
                    match be.set_duty(i, d) {
                        Ok(()) => {
                            st.applied = Some(d);
                            st.last_write = now;
                            st.fails = 0;
                            st.error = None;
                        }
                        Err(e) => {
                            st.fails += 1;
                            st.error = Some(e.to_string());
                            if st.fails >= MAX_WRITE_FAILS {
                                let _ = be.release(i);
                                st.applied = None;
                                st.fault = Some(fan);
                                st.reason = Reason::Fault;
                            }
                        }
                    }
                }
            }
        }

        let rpm = info.rpm_fan.and_then(|f| self.fans[f]);
        let stalling = st.applied.is_some_and(|d| d >= STALL_MIN_DUTY) && rpm == Some(0.0);
        st.stall_secs = if stalling { st.stall_secs + dt } else { 0.0 };
        if is_calib && (st.error.is_some() || st.fault.is_some()) {
            self.finish_calibration(Some("stopped: writing to the fan failed".into()));
        }
    }

    /// Send each zone its wanted effect, but only when it differs from what it already shows
    /// (effects run on the devices, so an unchanged effect is never re-sent). Zones the config
    /// leaves alone are never written.
    fn apply_lights(&mut self, now: Instant) {
        for (k, &(b, i)) in self.light_loc.iter().enumerate() {
            let info = &self.inv.lights[k];
            let st = &mut self.lights[k];
            let Some(want) = self.cfg.lighting.wanted(&info.id).and_then(|e| e.fit(&info.kinds)) else {
                // Left alone: forget what we sent, so taking it back later re-sends.
                *st = LightState::default();
                continue;
            };
            if st.applied == Some(want) || st.failed.is_some_and(|(e, t)| e == want && now < t) {
                continue;
            }
            match self.backends[b].set_light(i, &want) {
                Ok(()) => {
                    *st = LightState {
                        applied: Some(want),
                        ..Default::default()
                    }
                }
                Err(e) => {
                    st.error = Some(e.to_string());
                    st.failed = Some((want, now + LIGHT_RETRY));
                }
            }
        }
    }

    /// "Save to device": every device that can store lighting, and shows some from fan-lite,
    /// stores what it shows now.
    pub fn save_lights(&mut self) {
        let mut save = LightSave {
            seq: self.light_save.as_ref().map_or(0, |s| s.seq) + 1,
            ..Default::default()
        };
        for (b, be) in self.backends.iter_mut().enumerate() {
            let used = self
                .light_loc
                .iter()
                .zip(&self.lights)
                .any(|(&(lb, _), st)| lb == b && st.applied.is_some());
            if !used || !be.can_save_lights() {
                continue;
            }
            match be.save_lights() {
                Ok(()) => save.saved.push(be.name().to_owned()),
                Err(e) => save.errors.push(format!("{}: {e}", be.name())),
            }
        }
        self.light_save = Some(save);
    }

    pub fn publish(&self, out: &Mutex<Snapshot>) {
        let mut s = out.lock().unwrap_or_else(|e| e.into_inner());
        if !Arc::ptr_eq(&s.inv, &self.inv) {
            s.inv = self.inv.clone();
        }
        s.ready = self.ready;
        s.tick = self.ticks;
        s.emergency = self.emergency;
        s.temps.clear();
        s.temps.extend(self.temps.iter().map(TempState::value));
        s.fans.clone_from(&self.fans);
        s.controls.clear();
        s.controls
            .extend(self.ctl.iter().zip(&self.duty).map(|(st, &duty)| ControlView {
                duty,
                commanded: st.applied,
                reason: st.reason,
                stalled: st.stall_secs >= STALL_SECS,
                error: st.error.clone(),
            }));
        s.curve_temps.clone_from(&self.curve_eff);
        s.fan_seen.clone_from(&self.fan_seen);
        s.calibrating = self.calib.as_ref().map(|c| {
            (
                self.inv.controls[c.k].id.clone(),
                c.steps[c.idx.min(c.steps.len() - 1)],
                c.idx + 1,
                c.steps.len(),
            )
        });
        s.last_calib.clone_from(&self.last_calib);
        s.lights.clear();
        s.lights.extend(self.lights.iter().map(|st| LightView {
            applied: st.applied,
            error: st.error.clone(),
        }));
        s.light_save.clone_from(&self.light_save);
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        for (k, &(b, i)) in self.ctl_loc.iter().enumerate() {
            if self.ctl[k].applied.is_some() || self.ctl[k].fault.is_some() {
                let _ = self.backends[b].release(i);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ZoneLight;
    use crate::config::{Curve, Mode};
    use crate::hw::{Channel, ControlChannel, HwError, LightZone};
    use crate::rgb::Rgb;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct Log {
        temp: Option<f32>,
        writes: Vec<f32>,
        releases: usize,
        fail_writes: bool,
        lights: Vec<Effect>,
        fail_lights: bool,
    }

    struct Mock {
        log: Rc<RefCell<Log>>,
        temps: Vec<Channel>,
        fans: Vec<Channel>,
        controls: Vec<ControlChannel>,
        lights: Vec<LightZone>,
    }

    impl Backend for Mock {
        fn name(&self) -> &'static str {
            "mock"
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
        fn update(&mut self, t: &mut [Option<f32>], f: &mut [Option<f32>], d: &mut [Option<f32>]) {
            let l = self.log.borrow();
            t[0] = l.temp;
            // Speed follows the last written duty (1000 rpm under firmware control).
            f[0] = Some(l.writes.last().map_or(1000.0, |w| w * 25.0 + 500.0));
            d[0] = l.writes.last().copied();
        }
        fn set_duty(&mut self, _: usize, pct: f32) -> Result<(), HwError> {
            let mut l = self.log.borrow_mut();
            if l.fail_writes {
                return Err("boom".into());
            }
            l.writes.push(pct);
            Ok(())
        }
        fn release(&mut self, _: usize) -> Result<(), HwError> {
            self.log.borrow_mut().releases += 1;
            Ok(())
        }
        fn lights(&self) -> &[LightZone] {
            &self.lights
        }
        fn set_light(&mut self, _: usize, e: &Effect) -> Result<(), HwError> {
            let mut l = self.log.borrow_mut();
            if l.fail_lights {
                return Err("busy".into());
            }
            l.lights.push(*e);
            Ok(())
        }
    }

    fn setup(mode: Mode) -> (Engine, Rc<RefCell<Log>>) {
        let log = Rc::new(RefCell::new(Log {
            temp: Some(60.0),
            ..Default::default()
        }));
        let mock = Mock {
            log: log.clone(),
            temps: vec![Channel {
                id: "t".into(),
                label: "T".into(),
            }],
            fans: vec![Channel {
                id: "f".into(),
                label: "F".into(),
            }],
            controls: vec![ControlChannel {
                id: "c".into(),
                label: "C".into(),
                hw_min_pct: 0.0,
                rpm_fan: Some(0),
                optional: false,
            }],
            lights: vec![LightZone {
                id: "z".into(),
                label: "Z".into(),
                leds: 8,
                kinds: vec![Kind::Static, Kind::Breathing, Kind::Rainbow],
            }],
        };
        let mut cfg = Config::default();
        cfg.curves.push(Curve {
            name: "k".into(),
            sensor: "t".into(),
            points: vec![(40.0, 0.0), (80.0, 100.0)],
            hysteresis: 0.0,
            smooth: 0.0,
        });
        let fan = cfg.fan_mut("c");
        fan.mode = mode;
        fan.min_pct = 0.0;
        (Engine::new(vec![Box::new(mock)], Vec::new(), cfg), log)
    }

    /// Pretend the current calibration step has already settled and been sampled.
    fn fast_forward(e: &mut Engine) {
        if let Some(c) = &mut e.calib {
            c.step_start -= CALIB_SETTLE + CALIB_SAMPLE + Duration::from_secs(1);
        }
    }

    #[test]
    fn calibration_sweeps_down_and_hands_back() {
        let (mut e, log) = setup(Mode::Auto);
        e.start_calibration("c");
        e.tick(); // writes the first step
        for _ in 0..CALIB_STEPS.len() {
            fast_forward(&mut e);
            e.tick();
        }
        assert!(e.calib.is_none());
        let r = e.last_calib.clone().unwrap();
        assert_eq!(r.aborted, None);
        let duties: Vec<f32> = r.results.iter().map(|p| p.0).collect();
        assert_eq!(duties, CALIB_STEPS.to_vec());
        for (d, rpm) in &r.results {
            assert_eq!(*rpm, d * 25.0 + 500.0, "rpm measured at the duty that was written");
        }
        assert!(log.borrow().writes.iter().all(|&w| w >= 20.0), "never below 20 %");
        // The fan was on Auto, so it goes back to firmware control.
        assert_eq!(log.borrow().releases, 1);
        assert_eq!(e.ctl[0].applied, None);
    }

    #[test]
    fn calibration_respects_floor() {
        let (mut e, _log) = setup(Mode::Auto);
        e.cfg.fan_mut("c").min_pct = 40.0;
        e.start_calibration("c");
        assert_eq!(e.calib.as_ref().unwrap().steps, vec![100.0, 80.0, 60.0, 45.0, 40.0]);
    }

    #[test]
    fn calibration_aborts_when_hot() {
        let (mut e, log) = setup(Mode::Manual(30.0));
        e.start_calibration("c");
        e.tick();
        log.borrow_mut().temp = Some(85.0); // within 10 °C of critical (90)
        e.tick();
        assert!(e.calib.is_none());
        assert!(e.last_calib.as_ref().unwrap().aborted.is_some());
        // Back under its configured manual mode (ramping down from the sweep, not jumping).
        log.borrow_mut().temp = Some(60.0);
        e.tick();
        assert_eq!(e.ctl[0].reason, Reason::Manual);
    }

    #[test]
    fn smoothing_lags_spikes_but_safety_does_not() {
        let (mut e, log) = setup(Mode::Curve("k".into()));
        e.cfg.curves[0].smooth = 5.0;
        e.read_all();
        e.update_safety_inputs(1.0);
        assert_eq!(e.curve_smooth[0].1, 60.0);
        log.borrow_mut().temp = Some(95.0); // sudden spike
        e.read_all();
        e.update_safety_inputs(1.0);
        let s = e.curve_smooth[0].1;
        assert!(s > 60.0 && s < 70.0, "curve input is smoothed: {s}");
        assert!(e.emergency, "emergency reacts to the raw temperature at once");
    }

    #[test]
    fn auto_never_writes() {
        let (mut e, log) = setup(Mode::Auto);
        e.tick();
        e.tick();
        assert!(log.borrow().writes.is_empty());
        drop(e);
        assert_eq!(log.borrow().releases, 0);
    }

    #[test]
    fn curve_writes_and_drop_restores() {
        let (mut e, log) = setup(Mode::Curve("k".into()));
        e.tick();
        assert_eq!(log.borrow().writes, vec![50.0]);
        drop(e);
        assert_eq!(log.borrow().releases, 1);
    }

    #[test]
    fn lost_sensor_goes_full_speed_after_misses() {
        let (mut e, log) = setup(Mode::Curve("k".into()));
        e.tick();
        log.borrow_mut().temp = None;
        for _ in 0..MAX_MISSES - 1 {
            e.tick();
        }
        assert_eq!(*log.borrow().writes.last().unwrap(), 50.0); // holds last good value
        e.tick();
        assert_eq!(*log.borrow().writes.last().unwrap(), 100.0);
        assert_eq!(e.ctl[0].reason, Reason::SensorFault);
    }

    #[test]
    fn emergency_forces_full_speed() {
        let (mut e, log) = setup(Mode::Manual(30.0));
        e.tick();
        assert_eq!(*log.borrow().writes.last().unwrap(), 30.0);
        log.borrow_mut().temp = Some(95.0);
        e.tick();
        assert_eq!(*log.borrow().writes.last().unwrap(), 100.0);
        assert!(e.emergency);
    }

    #[test]
    fn repeated_write_failures_release_and_fault() {
        let (mut e, log) = setup(Mode::Manual(40.0));
        log.borrow_mut().fail_writes = true;
        for _ in 0..MAX_WRITE_FAILS {
            e.tick();
        }
        assert_eq!(e.ctl[0].reason, Reason::Fault);
        assert_eq!(log.borrow().releases, 1);
        // Stays faulted (no further attempts) until settings change.
        log.borrow_mut().fail_writes = false;
        e.tick();
        assert!(log.borrow().writes.is_empty());
        e.cfg.fan_mut("c").mode = Mode::Manual(45.0);
        e.tick();
        assert_eq!(log.borrow().writes, vec![45.0]);
    }

    #[test]
    fn switching_to_auto_releases() {
        let (mut e, log) = setup(Mode::Manual(40.0));
        e.tick();
        e.cfg.fan_mut("c").mode = Mode::Auto;
        e.tick();
        assert_eq!(log.borrow().releases, 1);
        assert_eq!(e.ctl[0].applied, None);
    }

    #[test]
    fn lights_untouched_unless_configured() {
        let (mut e, log) = setup(Mode::Auto);
        e.tick();
        e.tick();
        assert!(log.borrow().lights.is_empty());
        e.cfg.lighting.sync = Some(Effect::default());
        e.cfg.lighting.zones.insert("z".into(), ZoneLight::Leave);
        e.tick();
        assert!(log.borrow().lights.is_empty(), "a zone set to Leave is never written");
    }

    #[test]
    fn sync_is_fitted_and_sent_only_on_change() {
        let (mut e, log) = setup(Mode::Auto);
        e.cfg.lighting.sync = Some(Effect {
            kind: Kind::Wave,
            ..Default::default()
        });
        e.tick();
        e.tick();
        e.tick();
        assert_eq!(log.borrow().lights.len(), 1, "unchanged effects are not re-sent");
        assert_eq!(
            log.borrow().lights[0].kind,
            Kind::Rainbow,
            "Wave fitted to what the zone supports"
        );
        e.cfg.lighting.zones.insert(
            "z".into(),
            ZoneLight::Own(Effect {
                kind: Kind::Static,
                colors: [Rgb(255, 0, 0); 2],
                ..Default::default()
            }),
        );
        e.tick();
        assert_eq!(log.borrow().lights.len(), 2);
        assert_eq!(log.borrow().lights[1].colors[0], Rgb(255, 0, 0));
    }

    #[test]
    fn failed_light_writes_wait_unless_the_effect_changes() {
        let (mut e, log) = setup(Mode::Auto);
        e.cfg.lighting.sync = Some(Effect::default());
        log.borrow_mut().fail_lights = true;
        e.tick();
        assert!(e.lights[0].error.is_some());
        log.borrow_mut().fail_lights = false;
        e.tick();
        assert!(log.borrow().lights.is_empty(), "same effect: waits LIGHT_RETRY");
        e.cfg.lighting.sync.as_mut().unwrap().brightness = 50;
        e.tick();
        assert_eq!(log.borrow().lights.len(), 1, "a new effect is tried at once");
        assert!(e.lights[0].error.is_none());
    }
}
