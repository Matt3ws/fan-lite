//! NVIDIA GPUs through NVML (nvml.dll ships with the driver). Loaded at runtime, so the app
//! works without an NVIDIA driver. Fan control needs administrator rights.

use std::ffi::{c_char, c_void};

use super::win::Library;
use super::{Backend, Channel, ControlChannel, HwError};

type Dev = *mut c_void;
type Ret = u32;
const SUCCESS: Ret = 0;
const NVML_TEMPERATURE_GPU: u32 = 0;

#[repr(C)]
struct FanSpeedInfo {
    version: u32,
    fan: u32,
    speed: u32,
}
const FAN_SPEED_INFO_V1: u32 = size_of::<FanSpeedInfo>() as u32 | (1 << 24);

struct Api {
    shutdown: unsafe extern "C" fn() -> Ret,
    get_temp: unsafe extern "C" fn(Dev, u32, *mut u32) -> Ret,
    get_fan: unsafe extern "C" fn(Dev, u32, *mut u32) -> Ret,
    set_fan: unsafe extern "C" fn(Dev, u32, u32) -> Ret,
    set_default_fan: unsafe extern "C" fn(Dev, u32) -> Ret,
    get_rpm: Option<unsafe extern "C" fn(Dev, *mut FanSpeedInfo) -> Ret>,
}

fn err(what: &str, r: Ret) -> HwError {
    let why = match r {
        1 => "NVML not initialized".to_owned(),
        3 => "not supported".to_owned(),
        4 => "no permission (run as administrator)".to_owned(),
        n => format!("NVML error {n}"),
    };
    HwError(format!("{what}: {why}"))
}

pub struct Nvml {
    api: Api,
    temps: Vec<Channel>,
    fans: Vec<Channel>,
    controls: Vec<ControlChannel>,
    temp_devs: Vec<Dev>,
    /// (device, fan index) for each control.
    ctl: Vec<(Dev, u32)>,
    /// (device, fan index) for each RPM channel.
    rpm: Vec<(Dev, u32)>,
    taken: Vec<bool>,
}

impl Nvml {
    pub fn detect() -> Result<Self, HwError> {
        let lib = Library::load("nvml.dll").ok_or("nvml.dll not found (no NVIDIA driver)")?;
        unsafe {
            macro_rules! sym {
                ($n:literal) => {
                    lib.sym(concat!($n, "\0").as_bytes())
                        .ok_or(concat!($n, " missing"))?
                };
            }
            let init: unsafe extern "C" fn() -> Ret = sym!("nvmlInit_v2");
            let count: unsafe extern "C" fn(*mut u32) -> Ret = sym!("nvmlDeviceGetCount_v2");
            let by_index: unsafe extern "C" fn(u32, *mut Dev) -> Ret = sym!("nvmlDeviceGetHandleByIndex_v2");
            let name: unsafe extern "C" fn(Dev, *mut c_char, u32) -> Ret = sym!("nvmlDeviceGetName");
            let num_fans: unsafe extern "C" fn(Dev, *mut u32) -> Ret = sym!("nvmlDeviceGetNumFans");
            let min_max: unsafe extern "C" fn(Dev, *mut u32, *mut u32) -> Ret = sym!("nvmlDeviceGetMinMaxFanSpeed");
            let api = Api {
                shutdown: sym!("nvmlShutdown"),
                get_temp: sym!("nvmlDeviceGetTemperature"),
                get_fan: sym!("nvmlDeviceGetFanSpeed_v2"),
                set_fan: sym!("nvmlDeviceSetFanSpeed_v2"),
                set_default_fan: sym!("nvmlDeviceSetDefaultFanSpeed_v2"),
                get_rpm: lib.sym(b"nvmlDeviceGetFanSpeedRPM\0"),
            };

            let r = init();
            if r != SUCCESS {
                return Err(err("init", r));
            }
            let mut me = Nvml {
                api,
                temps: Vec::new(),
                fans: Vec::new(),
                controls: Vec::new(),
                temp_devs: Vec::new(),
                ctl: Vec::new(),
                rpm: Vec::new(),
                taken: Vec::new(),
            };
            let mut n = 0u32;
            let r = count(&mut n);
            if r != SUCCESS {
                return Err(err("device count", r));
            }
            for i in 0..n {
                let mut dev: Dev = std::ptr::null_mut();
                if by_index(i, &mut dev) != SUCCESS {
                    continue;
                }
                let mut buf = [0 as c_char; 96];
                let label = if name(dev, buf.as_mut_ptr(), buf.len() as u32) == SUCCESS {
                    std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
                } else {
                    format!("GPU {i}")
                };
                let mut t = 0u32;
                if (me.api.get_temp)(dev, NVML_TEMPERATURE_GPU, &mut t) == SUCCESS {
                    me.temps.push(Channel {
                        id: format!("nvml/{i}/temp"),
                        label: label.clone(),
                    });
                    me.temp_devs.push(dev);
                }
                let mut fans = 0u32;
                if num_fans(dev, &mut fans) != SUCCESS {
                    continue;
                }
                let (mut lo, mut hi) = (0u32, 100u32);
                if min_max(dev, &mut lo, &mut hi) != SUCCESS {
                    lo = 30; // conservative default when the driver won't say
                }
                for f in 0..fans {
                    let mut rpm_fan = None;
                    if let Some(get_rpm) = me.api.get_rpm {
                        let mut info = FanSpeedInfo {
                            version: FAN_SPEED_INFO_V1,
                            fan: f,
                            speed: 0,
                        };
                        if get_rpm(dev, &mut info) == SUCCESS {
                            rpm_fan = Some(me.fans.len());
                            me.fans.push(Channel {
                                id: format!("nvml/{i}/fan{f}/rpm"),
                                label: format!("{label} fan {}", f + 1),
                            });
                            me.rpm.push((dev, f));
                        }
                    }
                    me.controls.push(ControlChannel {
                        id: format!("nvml/{i}/fan{f}"),
                        label: format!("{label} fan {}", f + 1),
                        hw_min_pct: lo as f32,
                        rpm_fan,
                        optional: false,
                    });
                    me.ctl.push((dev, f));
                    me.taken.push(false);
                }
            }
            if me.temps.is_empty() && me.controls.is_empty() {
                return Err("no usable NVIDIA GPU".into());
            }
            Ok(me)
        }
    }
}

impl Backend for Nvml {
    fn name(&self) -> &'static str {
        "NVIDIA"
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
        unsafe {
            for (slot, &dev) in temps.iter_mut().zip(&self.temp_devs) {
                let mut t = 0u32;
                *slot = ((self.api.get_temp)(dev, NVML_TEMPERATURE_GPU, &mut t) == SUCCESS).then_some(t as f32);
            }
            if let Some(get_rpm) = self.api.get_rpm {
                for (slot, &(dev, f)) in fans.iter_mut().zip(&self.rpm) {
                    let mut info = FanSpeedInfo {
                        version: FAN_SPEED_INFO_V1,
                        fan: f,
                        speed: 0,
                    };
                    *slot = (get_rpm(dev, &mut info) == SUCCESS).then_some(info.speed as f32);
                }
            }
            for (slot, &(dev, f)) in duty.iter_mut().zip(&self.ctl) {
                let mut p = 0u32;
                *slot = ((self.api.get_fan)(dev, f, &mut p) == SUCCESS).then_some(p as f32);
            }
        }
    }

    fn set_duty(&mut self, i: usize, pct: f32) -> Result<(), HwError> {
        let (dev, f) = self.ctl[i];
        let v = pct.clamp(self.controls[i].hw_min_pct, 100.0).round() as u32;
        let r = unsafe { (self.api.set_fan)(dev, f, v) };
        if r != SUCCESS {
            return Err(err("set fan speed", r));
        }
        self.taken[i] = true;
        Ok(())
    }

    fn release(&mut self, i: usize) -> Result<(), HwError> {
        if !self.taken[i] {
            return Ok(());
        }
        let (dev, f) = self.ctl[i];
        let r = unsafe { (self.api.set_default_fan)(dev, f) };
        if r != SUCCESS {
            return Err(err("restore default fan control", r));
        }
        self.taken[i] = false;
        Ok(())
    }
}

impl Drop for Nvml {
    fn drop(&mut self) {
        for i in 0..self.ctl.len() {
            let _ = self.release(i);
        }
        unsafe { (self.api.shutdown)() };
    }
}
