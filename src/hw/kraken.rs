//! NZXT Kraken AIO coolers (Kraken 2023, Kraken Elite 2023, Kraken Z3) over USB HID:
//! liquid temperature, pump and radiator-fan speed and control.
//!
//! Protocol as implemented by liquidctl (`liquidctl/driver/kraken3.py`) and the Linux
//! `nzxt-kraken3` hwmon driver. 64-byte reports whose first byte is the report ID. The device
//! evaluates speed *curves* itself against its liquid temperature (20..=59 °C), so a fixed
//! duty is sent as a flat curve. Only one program may talk to it at a time (not with NZXT CAM).

use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HIDP_CAPS, HIDP_STATUS_SUCCESS, HidD_FreePreparsedData, HidD_GetHidGuid, HidD_GetPreparsedData, HidP_GetCaps,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};

use super::{Backend, Channel, ControlChannel, HwError};

const VID: &str = "vid_1e71";
/// Models that speak this protocol: (lower-case PID in the device path, name).
const MODELS: [(&str, &str); 3] = [
    ("pid_300c", "NZXT Kraken Elite"),
    ("pid_300e", "NZXT Kraken"),
    ("pid_3008", "NZXT Kraken Z3"),
];
/// The control/status interface; MI_00 is the LCD's bulk interface.
const HID_INTERFACE: &str = "mi_01";
const REPORT: usize = 64;
const READ_TIMEOUT_MS: u32 = 150;
const WRITE_TIMEOUT_MS: u32 = 500;
const MAX_READS: usize = 12;
/// The firmware can lock up or drop changes if curves arrive too fast.
const MIN_CURVE_GAP: Duration = Duration::from_millis(250);
/// How often to check whether NZXT CAM has started (it would fight us for the device).
const CAM_RECHECK: Duration = Duration::from_secs(5);
/// Lowest pump duty the protocol allows.
pub const PUMP_MIN: f32 = 20.0;

/// Curves the Kraken runs on its own (against liquid °C) when fan-lite hands a channel back.
const PUMP_RELEASE: [(f32, f32); 4] = [(30.0, 40.0), (35.0, 60.0), (40.0, 80.0), (45.0, 100.0)];
const FAN_RELEASE: [(f32, f32); 4] = [(30.0, 30.0), (35.0, 45.0), (40.0, 70.0), (45.0, 100.0)];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Status {
    /// `None` when the firmware reports a fault (`FF FF`: often missing SATA power).
    pub liquid: Option<f32>,
    pub pump_rpm: f32,
    pub pump_duty: f32,
    pub fan_rpm: f32,
    pub fan_duty: f32,
}

/// Decode a status reply (`75 01 …`). `None` for any other report.
pub fn parse_status(b: &[u8]) -> Option<Status> {
    if b.len() < 26 || b[0] != 0x75 || b[1] != 0x01 {
        return None;
    }
    let liquid = (!(b[15] == 0xFF && b[16] == 0xFF)).then(|| b[15] as f32 + b[16] as f32 / 10.0);
    Some(Status {
        liquid,
        pump_rpm: u16::from_le_bytes([b[17], b[18]]) as f32,
        pump_duty: b[19] as f32,
        fan_rpm: u16::from_le_bytes([b[23], b[24]]) as f32,
        fan_duty: b[25] as f32,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chan {
    Pump,
    Fan,
}

impl Chan {
    fn min(self) -> f32 {
        match self {
            Chan::Pump => PUMP_MIN,
            Chan::Fan => 0.0,
        }
    }
}

/// Full speed-curve command for liquid 20..=59 °C, padded to one report.
pub fn curve_cmd(chan: Chan, duties: &[u8; 40]) -> [u8; REPORT] {
    let mut b = [0u8; REPORT];
    // Header bytes 2..4 are specific to the 2023 models (0x00 0x00 on the Z3 also works for the pump).
    let header: [u8; 4] = match chan {
        Chan::Pump => [0x72, 0x01, 0x01, 0x00],
        Chan::Fan => [0x72, 0x02, 0x01, 0x01],
    };
    b[..4].copy_from_slice(&header);
    b[4..44].copy_from_slice(duties);
    b
}

/// A fixed duty as a flat curve, with the device-side failsafe of 100 % at 59 °C liquid.
pub fn flat_curve(chan: Chan, pct: f32) -> [u8; 40] {
    let d = pct.clamp(chan.min(), 100.0).round() as u8;
    let mut c = [d; 40];
    c[39] = 100;
    c
}

/// Interpolate (°C, %) points onto the device's 20..=59 °C grid: flat beyond the ends,
/// never decreasing, clamped to the channel's range, and 100 % at 59 °C.
pub fn liquid_curve(chan: Chan, points: &[(f32, f32)]) -> [u8; 40] {
    let mut out = [0u8; 40];
    let mut max = 0u8;
    for (i, slot) in out.iter_mut().enumerate() {
        let t = 20.0 + i as f32;
        let v = match points.iter().position(|p| p.0 >= t) {
            None => points.last().map_or(100.0, |p| p.1),
            Some(0) => points[0].1,
            Some(j) => {
                let (a, b) = (points[j - 1], points[j]);
                a.1 + (b.1 - a.1) * (t - a.0) / (b.0 - a.0)
            }
        };
        let v = v.clamp(chan.min(), 100.0).round() as u8;
        max = max.max(v);
        *slot = max;
    }
    out[39] = 100;
    out
}

/// True if NZXT CAM is running: it owns the device and would fight over it.
fn cam_running() -> bool {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        let mut ok = Process32FirstW(snap, &mut e) != 0;
        while ok && !found {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            let name = String::from_utf16_lossy(&e.szExeFile[..len]).to_ascii_lowercase();
            found = name.starts_with("nzxt cam");
            ok = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
        found
    }
}

/// Device path and model name of the first supported Kraken's control interface.
fn find_device() -> Option<(Vec<u16>, &'static str)> {
    unsafe {
        let mut guid = std::mem::zeroed();
        HidD_GetHidGuid(&mut guid);
        let set = SetupDiGetClassDevsW(&guid, null(), null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
        if set as isize == -1 {
            return None;
        }
        let mut found = None;
        let mut idx = 0;
        loop {
            let mut iface: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            iface.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(set, null(), &guid, idx, &mut iface) == 0 {
                break;
            }
            idx += 1;
            let mut need = 0u32;
            SetupDiGetDeviceInterfaceDetailW(set, &iface, null_mut(), 0, &mut need, null_mut());
            if need < 8 {
                continue;
            }
            // SP_DEVICE_INTERFACE_DETAIL_DATA_W: u32 cbSize (8 on x64) then a UTF-16 path.
            let mut buf = vec![0u32; (need as usize).div_ceil(4)];
            buf[0] = if cfg!(target_pointer_width = "64") { 8 } else { 6 };
            if SetupDiGetDeviceInterfaceDetailW(set, &iface, buf.as_mut_ptr().cast(), need, null_mut(), null_mut()) == 0
            {
                continue;
            }
            let wide = std::slice::from_raw_parts(buf.as_ptr().cast::<u16>().add(2), (need as usize - 4) / 2);
            let len = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
            let path = String::from_utf16_lossy(&wide[..len]).to_ascii_lowercase();
            if !path.contains(VID) || !path.contains(HID_INTERFACE) {
                continue;
            }
            if let Some(&(_, model)) = MODELS.iter().find(|(pid, _)| path.contains(pid)) {
                let mut w = wide[..len].to_vec();
                w.push(0);
                found = Some((w, model));
                break;
            }
        }
        SetupDiDestroyDeviceInfoList(set);
        found
    }
}

/// An open HID handle with overlapped I/O, so a stuck device can never block the engine.
struct Hid {
    h: HANDLE,
    ev: HANDLE,
    in_len: usize,
    out_len: usize,
}

impl Hid {
    fn open(path: &[u16]) -> Result<Self, HwError> {
        unsafe {
            let h = CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                null_mut(),
            );
            if h == INVALID_HANDLE_VALUE {
                return Err(HwError(format!("cannot open the Kraken (error {})", GetLastError())));
            }
            let ev = CreateEventW(null(), 1, 0, null());
            let mut hid = Hid {
                h,
                ev,
                in_len: REPORT,
                out_len: REPORT,
            };
            let mut pre = 0;
            if HidD_GetPreparsedData(h, &mut pre) {
                let mut caps: HIDP_CAPS = std::mem::zeroed();
                if HidP_GetCaps(pre, &mut caps) == HIDP_STATUS_SUCCESS {
                    hid.in_len = (caps.InputReportByteLength as usize).max(REPORT);
                    hid.out_len = (caps.OutputReportByteLength as usize).max(REPORT);
                }
                HidD_FreePreparsedData(pre);
            }
            Ok(hid)
        }
    }

    /// Run one overlapped transfer, giving up (and cancelling) after `timeout_ms`.
    unsafe fn transfer(&self, write: bool, buf: *mut u8, len: usize, timeout_ms: u32) -> Option<usize> {
        unsafe {
            ResetEvent(self.ev);
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = self.ev;
            let ok = if write {
                WriteFile(self.h, buf, len as u32, null_mut(), &mut ov)
            } else {
                ReadFile(self.h, buf, len as u32, null_mut(), &mut ov)
            };
            if ok == 0 && GetLastError() != ERROR_IO_PENDING {
                return None;
            }
            let mut n = 0u32;
            if WaitForSingleObject(self.ev, timeout_ms) == WAIT_OBJECT_0 {
                (GetOverlappedResult(self.h, &ov, &mut n, 0) != 0).then_some(n as usize)
            } else {
                // Cancel and wait for the cancellation, so `ov`/`buf` outlive the operation.
                CancelIoEx(self.h, &ov);
                GetOverlappedResult(self.h, &ov, &mut n, 1);
                None
            }
        }
    }

    fn write(&self, report: &[u8; REPORT]) -> Result<(), HwError> {
        let mut buf = vec![0u8; self.out_len];
        buf[..REPORT].copy_from_slice(report);
        match unsafe { self.transfer(true, buf.as_mut_ptr(), buf.len(), WRITE_TIMEOUT_MS) } {
            Some(_) => Ok(()),
            None => Err("writing to the Kraken failed or timed out".into()),
        }
    }

    fn read(&self, timeout_ms: u32) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; self.in_len];
        let n = unsafe { self.transfer(false, buf.as_mut_ptr(), buf.len(), timeout_ms) }?;
        buf.truncate(n);
        Some(buf)
    }

    /// Drop queued input reports so the next read is a reply to our next request.
    fn flush(&self) {
        for _ in 0..16 {
            if self.read(0).is_none() {
                break;
            }
        }
    }

    fn init(&self) -> Result<(), HwError> {
        self.flush();
        let mut a = [0u8; REPORT];
        a[..5].copy_from_slice(&[0x70, 0x02, 0x01, 0xB8, 0x01]);
        self.write(&a)?;
        let mut b = [0u8; REPORT];
        b[..2].copy_from_slice(&[0x70, 0x01]);
        self.write(&b)
    }

    fn status(&self) -> Result<Status, HwError> {
        self.flush();
        let mut req = [0u8; REPORT];
        req[..2].copy_from_slice(&[0x74, 0x01]);
        self.write(&req)?;
        for _ in 0..MAX_READS {
            match self.read(READ_TIMEOUT_MS) {
                Some(r) => {
                    if let Some(s) = parse_status(&r) {
                        return Ok(s);
                    }
                }
                None => break,
            }
        }
        Err("no status reply from the Kraken".into())
    }
}

impl Drop for Hid {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.h);
            CloseHandle(self.ev);
        }
    }
}

pub struct Kraken {
    path: Vec<u16>,
    model: &'static str,
    hid: Option<Hid>,
    temps: Vec<Channel>,
    fans: Vec<Channel>,
    controls: Vec<ControlChannel>,
    /// Last curve sent on *any* channel: back-to-back pump and fan curves can make the
    /// firmware drop one, so the gap applies across channels.
    last_curve: Option<Instant>,
    /// Channels fan-lite has written to (and must hand back on release).
    taken: [bool; 2],
    /// When NZXT CAM was last looked for, and whether it was running then.
    cam_checked: Instant,
    cam: bool,
}

const CHANS: [Chan; 2] = [Chan::Pump, Chan::Fan];

impl Kraken {
    pub fn detect() -> Result<Self, HwError> {
        let Some((path, model)) = find_device() else {
            return Err("no NZXT Kraken found".into());
        };
        if cam_running() {
            return Err(HwError(format!(
                "{model} found, but NZXT CAM is running - close it to let fan-lite control it"
            )));
        }
        let hid = Hid::open(&path)?;
        hid.init()?;
        hid.status()?;
        Ok(Kraken {
            path,
            model,
            hid: Some(hid),
            temps: vec![Channel {
                id: "kraken/liquid".into(),
                label: "Liquid".into(),
            }],
            fans: vec![
                Channel {
                    id: "kraken/pump/rpm".into(),
                    label: "Pump".into(),
                },
                Channel {
                    id: "kraken/fan/rpm".into(),
                    label: "Radiator fans".into(),
                },
            ],
            controls: vec![
                ControlChannel {
                    id: "kraken/pump".into(),
                    label: "Pump".into(),
                    hw_min_pct: PUMP_MIN,
                    rpm_fan: Some(0),
                    optional: false,
                },
                ControlChannel {
                    id: "kraken/fan".into(),
                    label: "Radiator fans".into(),
                    hw_min_pct: 0.0,
                    rpm_fan: Some(1),
                    optional: true,
                },
            ],
            last_curve: None,
            taken: [false; 2],
            cam_checked: Instant::now(),
            cam: false,
        })
    }

    /// The open device, re-opening and re-initialising it if needed (e.g. after sleep).
    fn device(&mut self) -> Result<&Hid, HwError> {
        // CAM may start after us (e.g. at login). Two programs talking to the Kraken steal each
        // other's replies and overwrite each other's curves, so back off while it runs.
        if self.cam_checked.elapsed() >= CAM_RECHECK {
            self.cam_checked = Instant::now();
            self.cam = cam_running();
        }
        if self.cam {
            self.hid = None;
            return Err("NZXT CAM is running - close it to let fan-lite control the Kraken".into());
        }
        if self.hid.is_none() {
            let hid = Hid::open(&self.path)?;
            hid.init()?;
            self.hid = Some(hid);
        }
        Ok(self.hid.as_ref().unwrap())
    }

    fn send_curve(&mut self, i: usize, duties: &[u8; 40]) -> Result<(), HwError> {
        if let Some(t) = self.last_curve {
            let since = t.elapsed();
            if since < MIN_CURVE_GAP {
                std::thread::sleep(MIN_CURVE_GAP - since);
            }
        }
        let cmd = curve_cmd(CHANS[i], duties);
        let r = self.device().and_then(|h| h.write(&cmd));
        self.last_curve = Some(Instant::now());
        if r.is_err() {
            self.hid = None; // re-open next time
        }
        r
    }
}

impl Backend for Kraken {
    fn name(&self) -> &'static str {
        self.model
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
        temps.fill(None);
        fans.fill(None);
        duty.fill(None);
        let status = self.device().and_then(|h| h.status());
        match status {
            Ok(s) => {
                temps[0] = s.liquid;
                fans[0] = Some(s.pump_rpm);
                fans[1] = Some(s.fan_rpm);
                duty[0] = Some(s.pump_duty);
                duty[1] = Some(s.fan_duty);
            }
            Err(_) => self.hid = None, // missed sample; re-open and re-init next tick
        }
    }

    fn set_duty(&mut self, i: usize, pct: f32) -> Result<(), HwError> {
        self.send_curve(i, &flat_curve(CHANS[i], pct))?;
        self.taken[i] = true;
        Ok(())
    }

    /// There is no firmware "auto" to return to, so leave the Kraken running its own
    /// liquid-temperature curve.
    fn release(&mut self, i: usize) -> Result<(), HwError> {
        if !self.taken[i] {
            return Ok(());
        }
        let pts: &[(f32, f32)] = if CHANS[i] == Chan::Pump {
            &PUMP_RELEASE
        } else {
            &FAN_RELEASE
        };
        self.send_curve(i, &liquid_curve(CHANS[i], pts))?;
        self.taken[i] = false;
        Ok(())
    }
}

impl Drop for Kraken {
    fn drop(&mut self) {
        for i in 0..CHANS.len() {
            let _ = self.release(i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn status_from_liquidctl_sample() {
        // Sample reply from liquidctl's tests (tests/test_kraken3.py).
        let mut b = hex("75012E0018001051393434363731011803690314140102000000");
        b.resize(64, 0);
        let s = parse_status(&b).unwrap();
        assert_eq!(s.liquid, Some(24.3));
        assert_eq!(s.pump_rpm, 873.0);
        assert_eq!(s.pump_duty, 20.0);
        assert_eq!((s.fan_rpm, s.fan_duty), (0.0, 0.0));
    }

    #[test]
    fn status_fault_and_wrong_report() {
        let mut b = vec![0u8; 64];
        b[0] = 0x75;
        b[1] = 0x01;
        b[15] = 0xFF;
        b[16] = 0xFF;
        assert_eq!(parse_status(&b).unwrap().liquid, None, "FF FF = sensor fault");
        b[1] = 0x02;
        assert_eq!(parse_status(&b), None);
        assert_eq!(parse_status(&[0x75, 0x01]), None, "too short");
    }

    #[test]
    fn speed_commands() {
        let p = curve_cmd(Chan::Pump, &flat_curve(Chan::Pump, 84.0));
        assert_eq!(p.len(), 64);
        assert_eq!(&p[..4], &[0x72, 0x01, 0x01, 0x00]);
        assert!(p[4..43].iter().all(|&d| d == 84));
        assert_eq!(p[43], 100, "failsafe at 59 °C");
        assert!(p[44..].iter().all(|&d| d == 0));
        let f = curve_cmd(Chan::Fan, &flat_curve(Chan::Fan, 0.0));
        assert_eq!(&f[..4], &[0x72, 0x02, 0x01, 0x01]);
        assert_eq!(f[4], 0, "fans may stop");
        assert_eq!(flat_curve(Chan::Pump, 10.0)[0], 20, "pump never below 20 %");
    }

    #[test]
    fn release_curves_are_sane() {
        for (chan, pts) in [(Chan::Pump, &PUMP_RELEASE[..]), (Chan::Fan, &FAN_RELEASE[..])] {
            let c = liquid_curve(chan, pts);
            assert!(c.windows(2).all(|w| w[0] <= w[1]), "monotonic");
            assert_eq!(c[39], 100);
            assert!(c[0] as f32 >= chan.min());
        }
        let pump = liquid_curve(Chan::Pump, &PUMP_RELEASE);
        assert_eq!(pump[10], 40, "30 °C liquid -> 40 %");
        assert_eq!(pump[25], 100, "45 °C liquid -> 100 %");
    }
}
