//! Build-time resources for fex.exe:
//! * the program icon, generated from `assets/fex_icon.png` at the sizes
//!   Windows asks for, embedded as an icon resource (what Explorer, shortcuts
//!   and the taskbar show for the file) plus a 64 px RGBA copy the window
//!   uses at run time (`OUT_DIR/icon_64.rgba`);
//! * link-time hardening (MSVC toolchain): the embedded `fex.exe.manifest`
//!   (asInvoker, long paths, common controls v6), and DLL imports resolved
//!   from System32 only, never from the folder fex.exe was started from,
//!   where a planted DLL could otherwise load.

use std::path::{Path, PathBuf};

const ICON: &str = "assets/fex_icon.png";
const ICON_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];
/// Size of the icon the window shows (title bar, taskbar, Alt+Tab).
const WINDOW_ICON: u32 = 64;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=fex.exe.manifest");
    println!("cargo:rerun-if-changed={ICON}");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    let (pixels, side) = load_square(Path::new(ICON));
    std::fs::write(out.join("icon_64.rgba"), resize(&pixels, side, WINDOW_ICON)).unwrap();

    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if !(windows && msvc) {
        return;
    }
    let images: Vec<(u32, Vec<u8>)> = ICON_SIZES.iter().map(|&n| (n, encode_png(&resize(&pixels, side, n), n))).collect();
    let res = out.join("fex_icon.res");
    std::fs::write(&res, icon_res(&images)).unwrap();
    // link.exe takes compiled resource files as inputs directly.
    println!("cargo:rustc-link-arg-bins={}", res.display());

    let manifest = Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("fex.exe.manifest");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());
    // The manifest declares the execution level itself.
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:NO");
    // LOAD_LIBRARY_SEARCH_SYSTEM32
    println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
}

/// The image as opaque RGBA, padded to a square with its corner colour.
fn load_square(path: &Path) -> (Vec<u8>, u32) {
    let file = std::io::BufReader::new(std::fs::File::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())));
    let mut decoder = png::Decoder::new(file);
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    let channels = info.color_type.samples();
    let (w, h) = (info.width, info.height);
    let at = |x: u32, y: u32| -> [u8; 4] {
        let p = &buf[y as usize * info.line_size + x as usize * channels..][..channels];
        match channels {
            1 => [p[0], p[0], p[0], 255],
            2 => [p[0], p[0], p[0], p[1]],
            3 => [p[0], p[1], p[2], 255],
            _ => [p[0], p[1], p[2], p[3]],
        }
    };
    let side = w.max(h);
    let fill = at(0, 0);
    let (dx, dy) = ((side - w) / 2, (side - h) / 2);
    let mut out = Vec::with_capacity((side * side * 4) as usize);
    for y in 0..side {
        for x in 0..side {
            let inside = x >= dx && x < dx + w && y >= dy && y < dy + h;
            out.extend(if inside { at(x - dx, y - dy) } else { fill });
        }
    }
    (out, side)
}

/// Area-average resample of a square RGBA image (each output pixel is the
/// mean of the source area it covers, which keeps small sizes crisp).
fn resize(src: &[u8], from: u32, to: u32) -> Vec<u8> {
    let scale = from as f64 / to as f64;
    // For each output index: (source index, weight) pairs.
    let taps: Vec<Vec<(usize, f64)>> = (0..to)
        .map(|i| {
            let (a, b) = (i as f64 * scale, (i + 1) as f64 * scale);
            (a.floor() as usize..(b.ceil() as usize).min(from as usize))
                .map(|s| (s, (b.min(s as f64 + 1.0) - a.max(s as f64)).max(0.0)))
                .filter(|&(_, w)| w > 0.0)
                .collect()
        })
        .collect();
    let mut out = Vec::with_capacity((to * to * 4) as usize);
    for ty in &taps {
        for tx in &taps {
            let mut acc = [0f64; 4];
            let mut total = 0.0;
            for &(sy, wy) in ty {
                for &(sx, wx) in tx {
                    let p = &src[(sy * from as usize + sx) * 4..][..4];
                    for c in 0..4 {
                        acc[c] += p[c] as f64 * wx * wy;
                    }
                    total += wx * wy;
                }
            }
            out.extend(acc.map(|v| (v / total).round().clamp(0.0, 255.0) as u8));
        }
    }
    out
}

fn encode_png(rgba: &[u8], side: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, side, side);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().unwrap().write_image_data(rgba).unwrap();
    bytes
}

/// A compiled resource (.res) file holding one icon group (ID 1, the one
/// Explorer shows) with an image per size. Each image is stored as PNG,
/// which Windows has accepted inside icon resources since Vista.
fn icon_res(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    const RT_ICON: u16 = 3;
    const RT_GROUP_ICON: u16 = 14;
    let mut res = Vec::new();
    entry(&mut res, 0, 0, 0, &[]); // every .res file starts with an empty entry
    for (i, (_, png)) in images.iter().enumerate() {
        entry(&mut res, RT_ICON, i as u16 + 1, 0x1010, png);
    }
    // GRPICONDIR: reserved, type 1 (icon), count; then a 14-byte entry per image.
    let mut group = Vec::new();
    for v in [0u16, 1, images.len() as u16] {
        group.extend(v.to_le_bytes());
    }
    for (i, (side, png)) in images.iter().enumerate() {
        let dim = if *side >= 256 { 0 } else { *side as u8 }; // 0 means 256
        group.extend([dim, dim, 0, 0]);
        group.extend(1u16.to_le_bytes()); // planes
        group.extend(32u16.to_le_bytes()); // bits per pixel
        group.extend((png.len() as u32).to_le_bytes());
        group.extend((i as u16 + 1).to_le_bytes());
    }
    entry(&mut res, RT_GROUP_ICON, 1, 0x1030, &group);
    res
}

/// One RESOURCEHEADER with numeric type and name, then its data, DWORD-aligned.
fn entry(res: &mut Vec<u8>, kind: u16, id: u16, flags: u16, data: &[u8]) {
    const LANG_EN_US: u16 = 0x0409;
    res.extend((data.len() as u32).to_le_bytes()); // DataSize
    res.extend(32u32.to_le_bytes()); // HeaderSize
    for v in [0xFFFF, kind, 0xFFFF, id] {
        res.extend(v.to_le_bytes()); // TYPE and NAME as ordinals
    }
    res.extend(0u32.to_le_bytes()); // DataVersion
    res.extend(flags.to_le_bytes()); // MemoryFlags
    res.extend(if kind == 0 { 0 } else { LANG_EN_US }.to_le_bytes());
    res.extend(0u32.to_le_bytes()); // Version
    res.extend(0u32.to_le_bytes()); // Characteristics
    res.extend(data);
    while res.len() % 4 != 0 {
        res.push(0);
    }
}
