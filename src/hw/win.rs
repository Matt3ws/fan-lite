//! Thin Win32 helpers: global named mutexes, dynamic library loading, process lookup,
//! elevation check.

use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HMODULE, INVALID_HANDLE_VALUE, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, InitializeSecurityDescriptor, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
    SetSecurityDescriptorDacl, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, MUTEX_MODIFY_STATE, OpenMutexW, OpenProcessToken, ReleaseMutex,
    SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Mutex for the LPC/Super I/O bus, shared with LibreHardwareMonitor, FanControl, HWiNFO, ASUS tools.
pub const ISA_BUS_MUTEX: &str = "Global\\Access_ISABUS.HTP.Method";
/// Mutex for PCI config space (SMN access on AMD).
pub const PCI_BUS_MUTEX: &str = "Global\\Access_PCI";

/// A named mutex shared across processes. Owned handle, closed on drop.
/// If it cannot be created or opened, locking always succeeds (same as LHM), so we
/// degrade to unsynchronised access rather than no access.
pub struct GlobalMutex(HANDLE);

impl GlobalMutex {
    /// Create (or open) the mutex with a NULL DACL so non-elevated tools can also use it.
    pub fn new(name: &str) -> Self {
        let w = wide(name);
        unsafe {
            let mut sd: SECURITY_DESCRIPTOR = std::mem::zeroed();
            let sd_ptr = &mut sd as *mut SECURITY_DESCRIPTOR as *mut c_void;
            let mut sa = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: null_mut(),
                bInheritHandle: 0,
            };
            if InitializeSecurityDescriptor(sd_ptr, SECURITY_DESCRIPTOR_REVISION) != 0
                && SetSecurityDescriptorDacl(sd_ptr, 1, null(), 0) != 0
            {
                sa.lpSecurityDescriptor = sd_ptr;
            }
            let mut h = CreateMutexW(&sa, 0, w.as_ptr());
            if h.is_null() {
                h = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE | MUTEX_MODIFY_STATE, 0, w.as_ptr());
            }
            GlobalMutex(h)
        }
    }

    /// Wait up to `timeout_ms`. An abandoned mutex (owner crashed) counts as acquired.
    pub fn lock(&self, timeout_ms: u32) -> Option<MutexGuard<'_>> {
        if self.0.is_null() {
            return Some(MutexGuard(self));
        }
        let r = unsafe { WaitForSingleObject(self.0, timeout_ms) };
        (r == WAIT_OBJECT_0 || r == WAIT_ABANDONED).then_some(MutexGuard(self))
    }
}

impl Drop for GlobalMutex {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

pub struct MutexGuard<'a>(&'a GlobalMutex);

impl Drop for MutexGuard<'_> {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            unsafe { ReleaseMutex(self.0.0) };
        }
    }
}

/// A loaded DLL. Never unloaded: function pointers taken from it stay valid for the process.
#[derive(Clone, Copy)]
pub struct Library(HMODULE);

impl Library {
    /// Load by absolute path, or by bare name from System32 only (avoids DLL search-path hijacking).
    pub fn load(path: &str) -> Option<Self> {
        let w = wide(path);
        let flags = if path.contains('\\') {
            0
        } else {
            LOAD_LIBRARY_SEARCH_SYSTEM32
        };
        let h = unsafe { LoadLibraryExW(w.as_ptr(), null_mut(), flags) };
        (!h.is_null()).then_some(Library(h))
    }

    /// Look up `name` (must be NUL-terminated) and cast it to the function pointer type `F`.
    ///
    /// # Safety
    /// `F` must be an `extern "system"` fn pointer matching the export's real signature.
    pub unsafe fn sym<F: Copy>(&self, name: &[u8]) -> Option<F> {
        debug_assert_eq!(name.last(), Some(&0));
        debug_assert_eq!(size_of::<F>(), size_of::<usize>());
        let p = unsafe { GetProcAddress(self.0, name.as_ptr()) }?;
        Some(unsafe { std::mem::transmute_copy(&p) })
    }
}

/// The first running process whose exe name starts with one of `prefixes` (lower case,
/// e.g. `"nzxt cam"`), if any. Used to back off while a vendor tool owns a device.
pub fn process_running(prefixes: &[&str]) -> Option<String> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut found = None;
        let mut ok = Process32FirstW(snap, &mut e) != 0;
        while ok && found.is_none() {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            let name = String::from_utf16_lossy(&e.szExeFile[..len]);
            let lower = name.to_ascii_lowercase();
            if prefixes.iter().any(|p| lower.starts_with(p)) {
                found = Some(name);
            }
            ok = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
        found
    }
}

pub fn is_elevated() -> bool {
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elev = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elev as *mut _ as *mut c_void,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        CloseHandle(token);
        ok != 0 && elev.TokenIsElevated != 0
    }
}
