//! PawnIO user-mode library wrapper (https://pawnio.eu).
//!
//! PawnIO is a signed kernel driver that runs small signed "modules" which expose a
//! restricted set of hardware operations (Super I/O port I/O, AMD SMN reads, ...).
//! We embed the official signed module blobs and load each into its own executor handle.

use std::ffi::c_void;
use std::ptr::null_mut;

use super::HwError;
use super::win::Library;

type Handle = *mut c_void;
type OpenFn = unsafe extern "system" fn(*mut Handle) -> i32;
type LoadFn = unsafe extern "system" fn(Handle, *const u8, usize) -> i32;
type ExecFn = unsafe extern "system" fn(Handle, *const u8, *const u64, usize, *mut u64, usize, *mut usize) -> i32;
type CloseFn = unsafe extern "system" fn(Handle) -> i32;

/// Signed module blobs from PawnIO.Modules 0.2.11 (LGPL-2.1, see third_party/pawnio/COPYING).
pub const LPC_IO_MODULE: &[u8] = include_bytes!("../../third_party/pawnio/LpcIO.bin");
pub const AMD_FAMILY17_MODULE: &[u8] = include_bytes!("../../third_party/pawnio/AMDFamily17.bin");

#[derive(Clone, Copy)]
pub struct PawnIo {
    open: OpenFn,
    load: LoadFn,
    exec: ExecFn,
    close: CloseFn,
}

fn hresult_msg(hr: i32) -> String {
    match hr as u32 {
        0x8007_0005 => "access denied (run as administrator)".into(),
        0x8007_0002 | 0x8007_0003 => "PawnIO driver not found (is it installed and running?)".into(),
        0x8007_0032 | 0x8000_4001 => "not supported on this hardware".into(),
        other => format!("error 0x{other:08X}"),
    }
}

impl PawnIo {
    pub fn load_library() -> Result<Self, HwError> {
        let base = std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".into());
        let path = format!("{base}\\PawnIO\\PawnIOLib.dll");
        let lib = Library::load(&path).ok_or("PawnIOLib.dll not found; install PawnIO from https://pawnio.eu")?;
        unsafe {
            Ok(PawnIo {
                open: lib.sym(b"pawnio_open\0").ok_or("pawnio_open missing")?,
                load: lib.sym(b"pawnio_load\0").ok_or("pawnio_load missing")?,
                exec: lib.sym(b"pawnio_execute\0").ok_or("pawnio_execute missing")?,
                close: lib.sym(b"pawnio_close\0").ok_or("pawnio_close missing")?,
            })
        }
    }

    /// Open an executor and load a signed module blob into it.
    pub fn module(&self, blob: &[u8]) -> Result<Module, HwError> {
        let mut h: Handle = null_mut();
        let hr = unsafe { (self.open)(&mut h) };
        if hr < 0 || h.is_null() {
            return Err(HwError(hresult_msg(hr)));
        }
        let m = Module { lib: *self, h };
        let hr = unsafe { (self.load)(h, blob.as_ptr(), blob.len()) };
        if hr < 0 {
            return Err(HwError(format!("module load failed: {}", hresult_msg(hr))));
        }
        Ok(m)
    }
}

pub struct Module {
    lib: PawnIo,
    h: Handle,
}

impl Module {
    /// Call `name` (NUL-terminated) with u64 inputs, filling `out`. Returns entries written.
    pub fn exec(&self, name: &[u8], input: &[u64], out: &mut [u64]) -> Result<usize, HwError> {
        debug_assert_eq!(name.last(), Some(&0));
        let mut written = 0usize;
        let hr = unsafe {
            (self.lib.exec)(
                self.h,
                name.as_ptr(),
                input.as_ptr(),
                input.len(),
                out.as_mut_ptr(),
                out.len(),
                &mut written,
            )
        };
        if hr < 0 {
            let n = String::from_utf8_lossy(&name[..name.len() - 1]);
            return Err(HwError(format!("{n}: {}", hresult_msg(hr))));
        }
        Ok(written)
    }

    /// Convenience for functions returning a single value.
    pub fn call1(&self, name: &[u8], input: &[u64]) -> Result<u64, HwError> {
        let mut out = [0u64; 1];
        self.exec(name, input, &mut out)?;
        Ok(out[0])
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        unsafe { (self.lib.close)(self.h) };
    }
}
