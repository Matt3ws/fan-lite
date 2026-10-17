//! USB HID devices (NZXT Kraken, ASUS Aura, Corsair hubs): finding them by VID/PID/interface
//! and talking to them with overlapped I/O, so a stuck device can never block the engine.

use std::ptr::{null, null_mut};

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
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};

use super::HwError;

const WRITE_TIMEOUT_MS: u32 = 500;

/// A present HID interface: its device path (NUL-terminated UTF-16) and the same path in
/// lower case, e.g. `\\?\hid#vid_1e71&pid_300c&mi_01#…`, for matching.
pub struct Found {
    pub path: Vec<u16>,
    pub lower: String,
}

/// Every present HID interface whose path contains `vid` (e.g. `"vid_1e71"`) and, if
/// given, `interface` (e.g. `"mi_01"`).
pub fn find(vid: &str, interface: Option<&str>) -> Vec<Found> {
    let mut out = Vec::new();
    unsafe {
        let mut guid = std::mem::zeroed();
        HidD_GetHidGuid(&mut guid);
        let set = SetupDiGetClassDevsW(&guid, null(), null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
        if set as isize == -1 {
            return out;
        }
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
            let lower = String::from_utf16_lossy(&wide[..len]).to_ascii_lowercase();
            if !lower.contains(vid) || interface.is_some_and(|i| !lower.contains(i)) {
                continue;
            }
            let mut path = wide[..len].to_vec();
            path.push(0);
            out.push(Found { path, lower });
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    out
}

/// An open HID handle with overlapped I/O.
pub struct Hid {
    h: HANDLE,
    ev: HANDLE,
    in_len: usize,
    out_len: usize,
    /// Name used in error messages.
    what: &'static str,
}

impl Hid {
    /// Open `path`. `min_len` is the report size the protocol uses (including the report ID);
    /// reports are padded to the device's own report length if that is larger.
    pub fn open(path: &[u16], min_len: usize, what: &'static str) -> Result<Self, HwError> {
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
                return Err(HwError(format!("cannot open the {what} (error {})", GetLastError())));
            }
            let ev = CreateEventW(null(), 1, 0, null());
            let mut hid = Hid {
                h,
                ev,
                in_len: min_len,
                out_len: min_len,
                what,
            };
            let mut pre = 0;
            if HidD_GetPreparsedData(h, &mut pre) {
                let mut caps: HIDP_CAPS = std::mem::zeroed();
                if HidP_GetCaps(pre, &mut caps) == HIDP_STATUS_SUCCESS {
                    hid.in_len = (caps.InputReportByteLength as usize).max(min_len);
                    hid.out_len = (caps.OutputReportByteLength as usize).max(min_len);
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

    /// Send one output report (`report[0]` is the report ID), zero-padded to the report length.
    pub fn write(&self, report: &[u8]) -> Result<(), HwError> {
        let mut buf = vec![0u8; self.out_len.max(report.len())];
        buf[..report.len()].copy_from_slice(report);
        match unsafe { self.transfer(true, buf.as_mut_ptr(), buf.len(), WRITE_TIMEOUT_MS) } {
            Some(_) => Ok(()),
            None => Err(HwError(format!("writing to the {} failed or timed out", self.what))),
        }
    }

    /// Output report length in bytes, including the report ID.
    pub fn out_len(&self) -> usize {
        self.out_len
    }

    pub fn read(&self, timeout_ms: u32) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; self.in_len];
        let n = unsafe { self.transfer(false, buf.as_mut_ptr(), buf.len(), timeout_ms) }?;
        buf.truncate(n);
        Some(buf)
    }

    /// Drop queued input reports so the next read is a reply to our next request.
    pub fn flush(&self) {
        for _ in 0..16 {
            if self.read(0).is_none() {
                break;
            }
        }
    }

    /// Write `req`, then read until `accept` returns a value (skipping unrelated reports),
    /// for at most `max_reads` reads of `timeout_ms` each.
    pub fn request<T>(
        &self,
        req: &[u8],
        timeout_ms: u32,
        max_reads: usize,
        mut accept: impl FnMut(&[u8]) -> Option<T>,
    ) -> Result<T, HwError> {
        self.flush();
        self.write(req)?;
        for _ in 0..max_reads {
            match self.read(timeout_ms) {
                Some(r) => {
                    if let Some(v) = accept(&r) {
                        return Ok(v);
                    }
                }
                None => break,
            }
        }
        Err(HwError(format!("no reply from the {}", self.what)))
    }
}

// The handles may be used from any thread; a `Hid` is only ever used by one at a time.
unsafe impl Send for Hid {}

impl Drop for Hid {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.h);
            CloseHandle(self.ev);
        }
    }
}
