//! The amber fan glyph as RGBA pixels: the tray icon, the window icon, and (through
//! `build.rs`, which includes this file) the icon embedded in FanLite.exe. Plain Rust with no
//! crate dependencies, so the build script can use it too.

use std::f32::consts::TAU;

/// The fan glyph from the UI (ring, five swept blades, hub) in amber, as straight-alpha RGBA,
/// rasterised with 4×4 supersampling. Used for both the tray and the window icon.
pub fn fan_icon_rgba(size: u32) -> Vec<u8> {
    const AMBER: [u8; 3] = [0xF2, 0xC9, 0x4C];
    const SS: u32 = 4;
    let r = 0.94f32;
    let blades: Vec<(f32, f32, f32, f32)> = (0..5)
        .map(|i| {
            let a = -TAU / 4.0 + i as f32 * TAU / 5.0;
            let tilt = a + 0.9;
            (a.cos() * r * 0.5, a.sin() * r * 0.5, tilt.cos(), tilt.sin())
        })
        .collect();
    let inside = |x: f32, y: f32| -> bool {
        let d = (x * x + y * y).sqrt();
        if d > r {
            return false;
        }
        if d >= r * 0.80 {
            return true; // ring
        }
        if d <= r * 0.10 {
            return false; // hole in the hub
        }
        if d <= r * 0.26 {
            return true; // hub
        }
        blades.iter().any(|&(cx, cy, c, s)| {
            let (dx, dy) = (x - cx, y - cy);
            let (u, v) = (dx * c + dy * s, -dx * s + dy * c);
            (u / (r * 0.42)).powi(2) + (v / (r * 0.19)).powi(2) <= 1.0
        })
    };
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = ((px * SS + sx) as f32 + 0.5) / (size * SS) as f32 * 2.0 - 1.0;
                    let y = ((py * SS + sy) as f32 + 0.5) / (size * SS) as f32 * 2.0 - 1.0;
                    hits += inside(x, y) as u32;
                }
            }
            let alpha = (hits * 255 / (SS * SS)) as u8;
            out.extend_from_slice(&[AMBER[0], AMBER[1], AMBER[2], alpha]);
        }
    }
    out
}
