//! Nuvoton NCT6797D / NCT6798D / NCT6799D Super I/O hardware monitor, through the PawnIO LpcIO module.
//!
//! Register tables follow LibreHardwareMonitor's `Nct677X.cs` and Linux `nct6775`.
//! Every register access is a bank select + index + data sequence done while holding the
//! global ISA bus mutex. The bank is re-selected on every access because firmware (ASUS ACPI)
//! may touch the chip between our accesses.

use std::thread::sleep;
use std::time::Duration;

use super::pawnio::{LPC_IO_MODULE, Module, PawnIo};
use super::win::{GlobalMutex, ISA_BUS_MUTEX};
use super::{Backend, Channel, ControlChannel, HwError};

const FAN_COUNT_REG: [u16; 7] = [0x4B0, 0x4B2, 0x4B4, 0x4B6, 0x4B8, 0x4BA, 0x4CC];
const PWM_OUT_REG: [u16; 7] = [0x001, 0x003, 0x011, 0x013, 0x015, 0xA09, 0xB09];
const PWM_CMD_REG: [u16; 7] = [0x109, 0x209, 0x309, 0x809, 0x909, 0xA09, 0xB09];
const MODE_REG: [u16; 7] = [0x102, 0x202, 0x302, 0x802, 0x902, 0xA02, 0xB02];
const VENDOR_ID_HI: u16 = 0x804F;
const VENDOR_ID_LO: u16 = 0x004F;
const NUVOTON_VENDOR_ID: u16 = 0x5CA3;

type TempSlot = (u16, Option<(u16, u8)>, u16);

/// Monitoring slots: (value reg, optional (half-degree reg, bit), source-select reg).
/// Each slot reports whichever temperature source its selector currently points at.
const TEMP_SLOTS: [TempSlot; 11] = [
    (0x073, Some((0x074, 7)), 0x100),
    (0x075, Some((0x076, 7)), 0x200),
    (0x077, Some((0x078, 7)), 0x300),
    (0x079, Some((0x07A, 7)), 0x800),
    (0x07B, Some((0x07C, 7)), 0x900),
    (0x07D, Some((0x07E, 7)), 0xA00),
    (0x4A0, Some((0x49E, 6)), 0xB00),
    (0x027, None, 0x621),
    (0x150, Some((0x151, 7)), 0x622),
    (0x670, None, 0xC26),
    (0x672, None, 0xC27),
];

fn source_label(src: u8) -> Option<String> {
    Some(match src {
        1 => "Motherboard (SYSTIN)".into(),
        2 => "VRM (CPUTIN)".into(),
        3..=7 => format!("AUXTIN{}", src - 3),
        8 | 9 => format!("SMBus master {}", src - 8),
        13 => "AUXTIN5".into(),
        16 => "CPU (TSI)".into(),
        17 => "TSI agent 1".into(),
        18..=21 => format!("PCH {}", src - 18),
        22..=25 => format!("DIMM {}", src - 22),
        26 | 27 => format!("Byte temp {}", src - 26),
        // 0, 12, 14, 15, 30: invalid; 10, 11, 31: virtual; 28, 29: undocumented calibration.
        _ => return None,
    })
}

/// RPM from the 13-bit count registers. `None` = no valid reading.
pub fn rpm_from_count(hi: u8, lo: u8) -> Option<f32> {
    let count = ((hi as u32) << 5) | (lo as u32 & 0x1F);
    if count >= 0x1FFF {
        Some(0.0)
    } else if count < 0x15 {
        None
    } else {
        Some(1.35e6 / count as f32)
    }
}

/// Temperature in °C from the main register and optional half-degree bit.
pub fn temp_from_regs(main: u8, half: Option<bool>) -> Option<f32> {
    let t = match half {
        Some(h) => (((main as i8 as i32) << 1) | h as i32) as f32 * 0.5,
        None => main as i8 as f32,
    };
    (-55.0..=125.0).contains(&t).then_some(t)
}

fn supported_chip(id: u16) -> Option<&'static str> {
    match id & 0xFFF8 {
        0xD450 => Some("NCT6797D"),
        0xD428 => Some("NCT6798D"),
        // 0xD806 is the NCT6701D, which shares the ID but has not been verified here.
        0xD800 if id != 0xD806 => Some("NCT6799D"),
        _ => None,
    }
}

pub struct Nct6799 {
    io: Module,
    mutex: GlobalMutex,
    index_port: u16,
    base: u16,
    chip: &'static str,
    temps: Vec<Channel>,
    /// Source number for each temp channel.
    temp_src: Vec<u8>,
    fans: Vec<Channel>,
    controls: Vec<ControlChannel>,
    /// Original (mode, pwm command) bytes, saved on first takeover.
    saved: [Option<(u8, u8)>; 7],
}

impl Nct6799 {
    pub fn detect(lib: &PawnIo) -> Result<Self, HwError> {
        let io = lib.module(LPC_IO_MODULE)?;
        let mutex = GlobalMutex::new(ISA_BUS_MUTEX);
        let guard = mutex.lock(100).ok_or("ISA bus busy (another tool holds it)")?;

        let mut found = None;
        let mut unsupported = Vec::new();
        for (slot, port) in [(0u64, 0x2Eu16), (1, 0x4E)] {
            io.exec(b"ioctl_select_slot\0", &[slot], &mut [])?;
            io.exec(b"ioctl_pio_outb\0", &[port as u64, 0x87], &mut [])?;
            io.exec(b"ioctl_pio_outb\0", &[port as u64, 0x87], &mut [])?;
            let id = io.call1(b"ioctl_superio_inw\0", &[0x20])? as u16;
            let result = match supported_chip(id) {
                Some(chip) => Self::init_chip(&io).map(|base| Some((port, base, chip))),
                None => {
                    if id != 0xFFFF && id != 0 {
                        unsupported.push(format!("0x{id:04X} at port 0x{port:02X}"));
                    }
                    Ok(None)
                }
            };
            io.exec(b"ioctl_pio_outb\0", &[port as u64, 0xAA], &mut [])?;
            found = result?;
            if found.is_some() {
                break;
            }
        }
        drop(guard);
        let Some((index_port, base, chip)) = found else {
            return Err(if unsupported.is_empty() {
                "no Super I/O chip found".into()
            } else {
                HwError(format!("unsupported chip {}", unsupported.join(", ")))
            });
        };

        let mut dev = Nct6799 {
            io,
            mutex,
            index_port,
            base,
            chip,
            temps: Vec::new(),
            temp_src: Vec::new(),
            fans: Vec::new(),
            controls: Vec::new(),
            saved: [None; 7],
        };
        let guard = dev.mutex.lock(100).ok_or("ISA bus busy (another tool holds it)")?;
        let vendor = ((dev.read(VENDOR_ID_HI)? as u16) << 8) | dev.read(VENDOR_ID_LO)? as u16;
        if vendor != NUVOTON_VENDOR_ID {
            return Err(HwError(format!("{chip}: unexpected vendor id 0x{vendor:04X}")));
        }
        // Discover which temperature sources the monitoring slots currently expose.
        let mut sources = Vec::new();
        for &(_, _, src_reg) in &TEMP_SLOTS {
            let src = dev.read(src_reg)? & 0x1F;
            if !sources.contains(&src) {
                sources.push(src);
            }
        }
        drop(guard);

        for src in sources {
            if let Some(label) = source_label(src) {
                dev.temp_src.push(src);
                dev.temps.push(Channel {
                    id: format!("nct/src{src}"),
                    label,
                });
            }
        }
        for i in 0..7 {
            dev.fans.push(Channel {
                id: format!("nct/fan{}", i + 1),
                label: format!("Fan #{}", i + 1),
            });
            dev.controls.push(ControlChannel {
                id: format!("nct/pwm{}", i + 1),
                label: format!("Fan #{}", i + 1),
                hw_min_pct: 0.0,
                rpm_fan: Some(i),
                optional: true,
            });
        }
        Ok(dev)
    }

    /// In config mode: select the HWM logical device, read and validate its base address,
    /// clear the I/O space lock and let the module allow-list the HWM ports.
    fn init_chip(io: &Module) -> Result<u16, HwError> {
        io.exec(b"ioctl_superio_outb\0", &[0x07, 0x0B], &mut [])?;
        let a = io.call1(b"ioctl_superio_inw\0", &[0x60])? as u16;
        sleep(Duration::from_millis(1));
        let b = io.call1(b"ioctl_superio_inw\0", &[0x60])? as u16;
        if a != b || a < 0x100 || a & 0xF007 != 0 {
            return Err(HwError(format!(
                "invalid hardware monitor base address 0x{a:04X}/0x{b:04X}"
            )));
        }
        if io.call1(b"ioctl_superio_inb\0", &[0x30])? & 1 == 0 {
            return Err("hardware monitor is disabled in firmware".into());
        }
        Self::unlock_io_space(io)?;
        io.exec(b"ioctl_find_bars\0", &[], &mut [])?;
        Ok(a)
    }

    fn unlock_io_space(io: &Module) -> Result<(), HwError> {
        let opts = io.call1(b"ioctl_superio_inb\0", &[0x28])?;
        if opts & 0x10 != 0 {
            io.exec(b"ioctl_superio_outb\0", &[0x28, opts & !0x10], &mut [])?;
        }
        Ok(())
    }

    /// Firmware may re-lock the HWM I/O space; redo the unlock in config mode.
    fn relock_recover(&self) -> Result<(), HwError> {
        let p = self.index_port as u64;
        self.io.exec(b"ioctl_pio_outb\0", &[p, 0x87], &mut [])?;
        self.io.exec(b"ioctl_pio_outb\0", &[p, 0x87], &mut [])?;
        let r = Self::unlock_io_space(&self.io);
        self.io.exec(b"ioctl_pio_outb\0", &[p, 0xAA], &mut [])?;
        r
    }

    fn select(&self, reg: u16) -> Result<(), HwError> {
        let addr = (self.base + 5) as u64;
        let data = (self.base + 6) as u64;
        self.io.exec(b"ioctl_pio_outb\0", &[addr, 0x4E], &mut [])?;
        self.io.exec(b"ioctl_pio_outb\0", &[data, (reg >> 8) as u64], &mut [])?;
        self.io
            .exec(b"ioctl_pio_outb\0", &[addr, (reg & 0xFF) as u64], &mut [])?;
        Ok(())
    }

    /// Caller must hold the ISA mutex.
    fn read(&self, reg: u16) -> Result<u8, HwError> {
        self.select(reg)?;
        Ok(self.io.call1(b"ioctl_pio_inb\0", &[(self.base + 6) as u64])? as u8)
    }

    /// Caller must hold the ISA mutex.
    fn write(&self, reg: u16, value: u8) -> Result<(), HwError> {
        self.select(reg)?;
        self.io
            .exec(b"ioctl_pio_outb\0", &[(self.base + 6) as u64, value as u64], &mut [])?;
        Ok(())
    }

    fn vendor_ok(&self) -> bool {
        matches!((self.read(VENDOR_ID_HI), self.read(VENDOR_ID_LO)),
            (Ok(h), Ok(l)) if ((h as u16) << 8 | l as u16) == NUVOTON_VENDOR_ID)
    }

    fn read_all(
        &self,
        temps: &mut [Option<f32>],
        fans: &mut [Option<f32>],
        duty: &mut [Option<f32>],
    ) -> Result<(), HwError> {
        for &(reg, half, src_reg) in &TEMP_SLOTS {
            let src = self.read(src_reg)? & 0x1F;
            let Some(ch) = self.temp_src.iter().position(|&s| s == src) else {
                continue;
            };
            if temps[ch].is_some() {
                continue; // first slot showing a source wins
            }
            let main = self.read(reg)?;
            let half = match half {
                Some((r, bit)) => Some(self.read(r)? >> bit & 1 == 1),
                None => None,
            };
            temps[ch] = temp_from_regs(main, half);
        }
        for (i, &reg) in FAN_COUNT_REG.iter().enumerate() {
            fans[i] = rpm_from_count(self.read(reg)?, self.read(reg + 1)?);
        }
        for (i, &reg) in PWM_OUT_REG.iter().enumerate() {
            duty[i] = Some(self.read(reg)? as f32 / 2.55);
        }
        Ok(())
    }
}

impl Backend for Nct6799 {
    fn name(&self) -> &'static str {
        self.chip
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
        let Some(_g) = self.mutex.lock(10) else { return };
        if !self.vendor_ok() {
            let _ = self.relock_recover();
            return;
        }
        if self.read_all(temps, fans, duty).is_err() {
            temps.fill(None);
            fans.fill(None);
            duty.fill(None);
        }
    }

    fn set_duty(&mut self, i: usize, pct: f32) -> Result<(), HwError> {
        let _g = self.mutex.lock(10).ok_or("ISA bus busy")?;
        if self.saved[i].is_none() {
            self.saved[i] = Some((self.read(MODE_REG[i])?, self.read(PWM_CMD_REG[i])?));
        }
        // Mode bits 7:4 = 0 selects manual mode; keep the tolerance bits 3:0 (as Linux does).
        let mode = self.read(MODE_REG[i])?;
        if mode & 0xF0 != 0 {
            self.write(MODE_REG[i], mode & 0x0F)?;
        }
        let raw = (pct.clamp(0.0, 100.0) * 2.55).round() as u8;
        self.write(PWM_CMD_REG[i], raw)
    }

    fn release(&mut self, i: usize) -> Result<(), HwError> {
        let Some((mode, cmd)) = self.saved[i] else {
            return Ok(());
        };
        let _g = self.mutex.lock(100).ok_or("ISA bus busy")?;
        self.write(MODE_REG[i], mode)?;
        self.write(PWM_CMD_REG[i], cmd)?;
        self.saved[i] = None;
        Ok(())
    }
}

impl Drop for Nct6799 {
    /// Last line of defence: never leave a header in manual mode when the backend goes away.
    fn drop(&mut self) {
        for i in 0..7 {
            let _ = self.release(i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpm_decode() {
        assert_eq!(rpm_from_count(0xFF, 0x1F), Some(0.0)); // 0x1FFF: stopped
        assert_eq!(rpm_from_count(0, 0x10), None); // below minimum count
        // count = 1000 -> 1350 rpm; 1000 = 31 << 5 | 8
        assert_eq!(rpm_from_count(31, 8), Some(1350.0));
        assert_eq!(rpm_from_count(31, 0xE8), Some(1350.0)); // upper bits of low byte ignored
    }

    #[test]
    fn temp_decode() {
        assert_eq!(temp_from_regs(45, Some(false)), Some(45.0));
        assert_eq!(temp_from_regs(45, Some(true)), Some(45.5));
        assert_eq!(temp_from_regs(0xFE, Some(true)), Some(-1.5)); // -2 + 0.5
        assert_eq!(temp_from_regs(38, None), Some(38.0));
        assert_eq!(temp_from_regs(0x7F, Some(true)), None); // 127.5: floating input
        assert_eq!(temp_from_regs(0xC2, None), None); // -62
    }

    #[test]
    fn chip_ids() {
        assert_eq!(supported_chip(0xD802), Some("NCT6799D"));
        assert_eq!(supported_chip(0xD806), None);
        assert_eq!(supported_chip(0xD42B), Some("NCT6798D"));
        assert_eq!(supported_chip(0xD451), Some("NCT6797D"));
        assert_eq!(supported_chip(0x8721), None);
    }
}
