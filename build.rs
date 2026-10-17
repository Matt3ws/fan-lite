// Embeds the UAC manifest and the exe's icon. Fan control through PawnIO and NVML needs admin
// rights, so the release binary always asks for elevation. Set FANLITE_NO_UAC=1 to build an
// unelevated binary for development (hardware access then degrades gracefully).

#[path = "src/ui/icon.rs"]
mod icon;

/// Icon sizes in the exe, for every place Explorer and the taskbar show it.
const ICON_SIZES: [u32; 9] = [16, 20, 24, 32, 40, 48, 64, 128, 256];

fn main() {
    println!("cargo:rerun-if-env-changed=FANLITE_NO_UAC");
    println!("cargo:rerun-if-changed=third_party/pawnio");
    println!("cargo:rerun-if-changed=src/ui/icon.rs");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if !msvc {
        return;
    }
    let level = if std::env::var_os("FANLITE_NO_UAC").is_some() {
        "asInvoker"
    } else {
        "requireAdministrator"
    };
    println!("cargo:rustc-link-arg-bin=fan-lite=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bin=fan-lite=/MANIFESTUAC:level='{level}' uiAccess='false'");

    // The icon as a compiled resource file, which link.exe takes as an input (no rc.exe needed).
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let res = out.join("icon.res");
    std::fs::write(&res, icon_res()).expect("write icon.res");
    println!("cargo:rustc-link-arg-bin=fan-lite={}", res.display());
}

/// One icon image: BITMAPINFOHEADER (height doubled for the AND mask), 32-bit BGRA rows
/// bottom-up, then the 1-bit AND mask (all zero: the alpha channel does the masking).
fn dib(size: u32) -> Vec<u8> {
    let rgba = icon::fan_icon_rgba(size);
    let mask_row = size.div_ceil(32) * 4;
    let mut b = Vec::new();
    for v in [40u32, size, size * 2] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(&1u16.to_le_bytes()); // planes
    b.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    for v in [0u32, size * size * 4 + mask_row * size, 0, 0, 0, 0] {
        b.extend_from_slice(&v.to_le_bytes()); // compression, image size, ppm x/y, colours
    }
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            b.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    b.resize(b.len() + (mask_row * size) as usize, 0);
    b
}

/// A `.res` file: an empty leading entry, one RT_ICON per size and the RT_GROUP_ICON (id 1,
/// which Explorer uses as the exe's icon) that lists them.
fn icon_res() -> Vec<u8> {
    const RT_ICON: u16 = 3;
    const RT_GROUP_ICON: u16 = 14;
    fn entry(out: &mut Vec<u8>, ty: u16, id: u16, flags: u16, data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&32u32.to_le_bytes()); // header size
        for w in [0xFFFF, ty, 0xFFFF, id] {
            out.extend_from_slice(&u16::to_le_bytes(w));
        }
        out.extend_from_slice(&0u32.to_le_bytes()); // data version
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&0x0409u16.to_le_bytes()); // language: en-US
        out.extend_from_slice(&[0; 8]); // version, characteristics
        out.extend_from_slice(data);
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
    }
    let mut out = Vec::new();
    entry(&mut out, 0, 0, 0, &[]);
    let mut group = Vec::new();
    for v in [0u16, 1, ICON_SIZES.len() as u16] {
        group.extend_from_slice(&v.to_le_bytes()); // reserved, type icon, count
    }
    for (i, &size) in ICON_SIZES.iter().enumerate() {
        let id = i as u16 + 1;
        let img = dib(size);
        entry(&mut out, RT_ICON, id, 0x1010, &img);
        let dim = if size >= 256 { 0 } else { size as u8 };
        group.extend_from_slice(&[dim, dim, 0, 0]); // width, height, colours, reserved
        group.extend_from_slice(&1u16.to_le_bytes()); // planes
        group.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        group.extend_from_slice(&(img.len() as u32).to_le_bytes());
        group.extend_from_slice(&id.to_le_bytes());
    }
    entry(&mut out, RT_GROUP_ICON, 1, 0x1030, &group);
    out
}
