//! AMD Ryzen (family 17h-1Ah) temperatures via SMN reads through the PawnIO AMDFamily17 module.
//! Register layout follows Linux k10temp and LibreHardwareMonitor's Amd17Cpu.

use super::pawnio::{AMD_FAMILY17_MODULE, Module, PawnIo};
use super::win::{GlobalMutex, PCI_BUS_MUTEX};
use super::{Backend, Channel, ControlChannel, HwError};

const THM_TCON_CUR_TMP: u64 = 0x0005_9800;
const CCD_TEMP_ZEN4: u64 = 0x0005_9B08; // Zen 4 / Zen 5 desktop
const CCD_TEMP_ZEN2: u64 = 0x0005_9954; // Zen 2 / Zen 3
const MAX_CCDS: usize = 8;

/// Tctl in °C from THM_TCON_CUR_TMP.
pub fn decode_tctl(raw: u32) -> f32 {
    let mut t = (raw >> 21) as f32 * 0.125;
    if raw & 0x8_0000 != 0 || raw & 0x3_0000 == 0x3_0000 {
        t -= 49.0;
    }
    t
}

/// CCD temperature in °C, or `None` if the CCD is absent.
pub fn decode_ccd(raw: u32) -> Option<f32> {
    let v = raw & 0xFFF;
    if v == 0 {
        return None;
    }
    let t = v as f32 * 0.125 - 305.0;
    (t < 125.0).then_some(t)
}

struct CpuId {
    family: u32,
    model: u32,
    amd: bool,
    brand: String,
}

fn cpuid() -> CpuId {
    use std::arch::x86_64::__cpuid;
    let v = __cpuid(0);
    let vendor: Vec<u8> = [v.ebx, v.edx, v.ecx].iter().flat_map(|r| r.to_le_bytes()).collect();
    let l1 = __cpuid(1).eax;
    let base_family = (l1 >> 8) & 0xF;
    let family = if base_family == 0xF {
        base_family + ((l1 >> 20) & 0xFF)
    } else {
        base_family
    };
    let model = ((l1 >> 12) & 0xF0) | ((l1 >> 4) & 0xF);
    let mut brand = Vec::with_capacity(48);
    for leaf in 0x8000_0002u32..=0x8000_0004 {
        let r = __cpuid(leaf);
        for x in [r.eax, r.ebx, r.ecx, r.edx] {
            brand.extend_from_slice(&x.to_le_bytes());
        }
    }
    CpuId {
        family,
        model,
        amd: vendor == b"AuthenticAMD",
        brand: String::from_utf8_lossy(&brand)
            .trim_matches(char::from(0))
            .trim()
            .to_owned(),
    }
}

/// Tctl reads high on a few early parts; k10temp subtracts these offsets to get Tdie.
fn tctl_offset(brand: &str) -> f32 {
    const TABLE: [(&str, f32); 6] = [
        ("1600X", 20.0),
        ("1700X", 20.0),
        ("1800X", 20.0),
        ("2700X", 10.0),
        ("Threadripper 19", 27.0),
        ("Threadripper 29", 27.0),
    ];
    TABLE.iter().find(|(k, _)| brand.contains(k)).map_or(0.0, |&(_, o)| o)
}

pub struct AmdCpu {
    module: Module,
    mutex: GlobalMutex,
    temps: Vec<Channel>,
    /// SMN addresses of present CCD sensors, in the same order as `temps[1..]`.
    ccds: Vec<u64>,
    offset: f32,
}

impl AmdCpu {
    pub fn detect(lib: &PawnIo) -> Result<Self, HwError> {
        let id = cpuid();
        if !id.amd || !(0x17..=0x1A).contains(&id.family) {
            return Err(HwError(format!("{} not supported (AMD family 17h-1Ah only)", id.brand)));
        }
        let module = lib.module(AMD_FAMILY17_MODULE)?;
        let mut cpu = AmdCpu {
            module,
            mutex: GlobalMutex::new(PCI_BUS_MUTEX),
            temps: vec![Channel {
                id: "amd/tctl".into(),
                label: "CPU Tctl".into(),
            }],
            ccds: Vec::new(),
            offset: tctl_offset(&id.brand),
        };
        if cpu.offset != 0.0 {
            cpu.temps[0].label = "CPU Tdie".into();
        }

        let ccd_base = match (id.family, id.model) {
            (0x19, 0x61) | (0x1A, 0x44) => Some(CCD_TEMP_ZEN4),
            (0x17, 0x31 | 0x71) | (0x19, 0x01 | 0x21) => Some(CCD_TEMP_ZEN2),
            _ => None,
        };
        let _g = cpu.mutex.lock(100);
        let tctl = cpu.read(THM_TCON_CUR_TMP)?;
        if !(-20.0..125.0).contains(&decode_tctl(tctl)) {
            return Err(HwError(format!("implausible Tctl reading 0x{tctl:08X}")));
        }
        if let Some(base) = ccd_base {
            for i in 0..MAX_CCDS {
                let addr = base + 4 * i as u64;
                if cpu.read(addr).ok().and_then(decode_ccd).is_some() {
                    cpu.ccds.push(addr);
                    cpu.temps.push(Channel {
                        id: format!("amd/ccd{}", i + 1),
                        label: format!("CPU CCD{}", i + 1),
                    });
                }
            }
        }
        drop(_g);
        Ok(cpu)
    }

    fn read(&self, addr: u64) -> Result<u32, HwError> {
        Ok(self.module.call1(b"ioctl_read_smn\0", &[addr])? as u32)
    }
}

impl Backend for AmdCpu {
    fn name(&self) -> &'static str {
        "AMD CPU"
    }
    fn temps(&self) -> &[Channel] {
        &self.temps
    }
    fn fans(&self) -> &[Channel] {
        &[]
    }
    fn controls(&self) -> &[ControlChannel] {
        &[]
    }

    fn update(&mut self, temps: &mut [Option<f32>], _: &mut [Option<f32>], _: &mut [Option<f32>]) {
        temps.fill(None);
        // Bus busy this tick: leave readings empty; the engine treats that as a missed sample.
        let Some(_g) = self.mutex.lock(10) else { return };
        temps[0] = self.read(THM_TCON_CUR_TMP).ok().map(|r| decode_tctl(r) - self.offset);
        for (slot, &addr) in temps[1..].iter_mut().zip(&self.ccds) {
            *slot = self.read(addr).ok().and_then(decode_ccd);
        }
    }

    fn set_duty(&mut self, _: usize, _: f32) -> Result<(), HwError> {
        Err("no controls".into())
    }
    fn release(&mut self, _: usize) -> Result<(), HwError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tctl_plain_and_offset_range() {
        // 45.0 °C: 45 / 0.125 = 360 -> raw = 360 << 21
        assert_eq!(decode_tctl(360 << 21), 45.0);
        // Range-select bit adds the -49 offset.
        assert_eq!(decode_tctl((752 << 21) | 0x8_0000), 45.0);
        assert_eq!(decode_tctl((752 << 21) | 0x3_0000), 45.0);
    }

    #[test]
    fn ccd_decode() {
        assert_eq!(decode_ccd(0), None);
        // (2800 * 0.125) - 305 = 45
        assert_eq!(decode_ccd(2800), Some(45.0));
        assert_eq!(decode_ccd(0xFFF), None); // 206.875 -> implausible
    }
}
