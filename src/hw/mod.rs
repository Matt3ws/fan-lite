//! Hardware abstraction: every sensor/fan source implements [`Backend`].
//!
//! Backends are created and used only on the engine thread, so they need not be `Send`.

pub mod amd_cpu;
pub mod hid;
pub mod kraken;
pub mod nct6799;
pub mod nvml;
pub mod pawnio;
pub mod win;

use std::fmt;

use crate::rgb::{Effect, Kind};

/// A read-only input (temperature or fan RPM).
#[derive(Clone, Debug)]
pub struct Channel {
    /// Stable identifier used in the config file, e.g. `nct/systin`.
    pub id: String,
    pub label: String,
}

/// A controllable fan output.
#[derive(Clone, Debug)]
pub struct ControlChannel {
    pub id: String,
    pub label: String,
    /// Lowest duty the hardware accepts (e.g. NVML's minimum), in percent.
    pub hw_min_pct: f32,
    /// Index into this backend's `fans()` of the matching RPM input, if any.
    pub rpm_fan: Option<usize>,
    /// True for motherboard headers, which may have nothing plugged in. GPU fans are
    /// always present (they read 0 rpm in zero-RPM mode), so they are never auto-hidden.
    pub optional: bool,
}

/// A lighting zone: one header, strip, fan ring, DIMM or the board's own LEDs.
#[derive(Clone, Debug)]
pub struct LightZone {
    /// Stable identifier used in the config file, e.g. `aura/argb1`.
    pub id: String,
    pub label: String,
    /// Number of LEDs, if the device reports it (0 = unknown or a plain 12 V RGB header).
    pub leds: u16,
    /// Effects this zone can run itself. The engine fits other effects onto these.
    pub kinds: Vec<Kind>,
}

#[derive(Debug, Clone)]
pub struct HwError(pub String);

impl fmt::Display for HwError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for HwError {
    fn from(s: String) -> Self {
        HwError(s)
    }
}

impl From<&str> for HwError {
    fn from(s: &str) -> Self {
        HwError(s.to_owned())
    }
}

pub trait Backend {
    fn name(&self) -> &'static str;
    fn temps(&self) -> &[Channel];
    fn fans(&self) -> &[Channel];
    fn controls(&self) -> &[ControlChannel];
    /// One batched read of everything. Slices have the lengths of `temps()`, `fans()`
    /// and `controls()`; `None` means "no valid reading this time".
    fn update(&mut self, temps: &mut [Option<f32>], fans: &mut [Option<f32>], duty: &mut [Option<f32>]);
    /// Take manual control of output `i` and set it to `pct` (0..=100).
    fn set_duty(&mut self, i: usize, pct: f32) -> Result<(), HwError>;
    /// Hand output `i` back to firmware/driver control. No-op if we never took it.
    fn release(&mut self, i: usize) -> Result<(), HwError>;

    /// Lighting zones (none for most backends).
    fn lights(&self) -> &[LightZone] {
        &[]
    }
    /// Run effect `e` on zone `i`. `e.kind` is always one of the zone's `kinds`.
    fn set_light(&mut self, _i: usize, _e: &Effect) -> Result<(), HwError> {
        Err("this device has no lighting".into())
    }
    /// Whether [`Self::save_lights`] can store lighting on the device.
    fn can_save_lights(&self) -> bool {
        false
    }
    /// Store the current lighting in the device's flash, so it survives power cycles
    /// without fan-lite. Only on request: flash wears out.
    fn save_lights(&mut self) -> Result<(), HwError> {
        Ok(())
    }
}

pub struct Detected {
    pub backends: Vec<Box<dyn Backend>>,
    pub diagnostics: Vec<String>,
}

/// Probe every supported backend. Failures become diagnostics, never panics.
pub fn detect_all() -> Detected {
    let mut backends: Vec<Box<dyn Backend>> = Vec::new();
    let mut diagnostics = Vec::new();

    if !win::is_elevated() {
        diagnostics.push("Not running as administrator: motherboard, CPU and fan control are unavailable".into());
    }

    match pawnio::PawnIo::load_library() {
        Ok(lib) => {
            match amd_cpu::AmdCpu::detect(&lib) {
                Ok(b) => backends.push(Box::new(b)),
                Err(e) => diagnostics.push(format!("CPU: {e}")),
            }
            match nct6799::Nct6799::detect(&lib) {
                Ok(b) => backends.push(Box::new(b)),
                Err(e) => diagnostics.push(format!("Super I/O: {e}")),
            }
        }
        Err(e) => diagnostics.push(format!("PawnIO: {e}")),
    }

    match nvml::Nvml::detect() {
        Ok(b) => backends.push(Box::new(b)),
        Err(e) => diagnostics.push(format!("NVIDIA: {e}")),
    }

    match kraken::Kraken::detect() {
        Ok(b) => backends.push(Box::new(b)),
        Err(e) => diagnostics.push(format!("AIO: {e}")),
    }

    match crate::rgb::aura_usb::AuraUsb::detect() {
        Ok(Some(b)) => backends.push(Box::new(b)),
        Ok(None) => {}
        Err(e) => diagnostics.push(format!("ASUS Aura: {e}")),
    }

    match crate::rgb::corsair_core::CommanderCore::detect() {
        Ok(Some(b)) => backends.push(Box::new(b)),
        Ok(None) => {}
        Err(e) => diagnostics.push(format!("Corsair: {e}")),
    }

    Detected { backends, diagnostics }
}
