use std::{env, fs, path::PathBuf};

const ICON_SIZES: [u32; 10] = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256];
const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;

fn main() {
    slint_build::compile("ui/library.slint").expect("compile Switchboard interface");
    println!("cargo:rerun-if-changed=ui/brand.svg");

    let svg = fs::read("ui/brand.svg").expect("read ui/brand.svg");
    let tree = resvg::usvg::Tree::from_data(&svg, &Default::default()).expect("parse brand.svg");
    let frames: Vec<(u32, Vec<u8>)> = ICON_SIZES.iter().map(|&s| (s, render(&tree, s))).collect();

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    // Cargo's build-dir layout is internal; the installer build reads OUT_DIR from
    // the build-script-executed message instead of guessing it.
    fs::write(out.join("switchboard.ico"), ico(&frames)).expect("write switchboard.ico");

    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        // link.exe accepts compiled .res files directly, so no resource compiler is needed.
        let res = out.join("switchboard.res");
        fs::write(&res, icon_resource(&frames)).expect("write switchboard.res");
        println!("cargo:rustc-link-arg-bins={}", res.display());
    }
}

fn render(tree: &resvg::usvg::Tree, size: u32) -> Vec<u8> {
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).expect("icon dimensions");
    let scale = size as f32 / tree.size().width().max(tree.size().height());
    resvg::render(
        tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap.encode_png().expect("encode icon frame")
}

fn put16(out: &mut Vec<u8>, values: &[u16]) {
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

/// ICONDIRENTRY and GRPICONDIRENTRY share this prefix; 256 px is stored as 0.
fn put_dir_entry(out: &mut Vec<u8>, size: u32, bytes: usize) {
    let dim = if size >= 256 { 0 } else { size as u8 };
    out.extend_from_slice(&[dim, dim, 0, 0]);
    put16(out, &[1, 32]);
    out.extend_from_slice(&(bytes as u32).to_le_bytes());
}

fn ico(frames: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![];
    put16(&mut out, &[0, 1, frames.len() as u16]);
    let mut offset = 6 + 16 * frames.len();
    for (size, png) in frames {
        put_dir_entry(&mut out, *size, png.len());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += png.len();
    }
    for (_, png) in frames {
        out.extend_from_slice(png);
    }
    out
}

/// One RESOURCEHEADER with ordinal type and name, followed by DWORD-aligned data.
fn put_resource(out: &mut Vec<u8>, kind: u16, id: u16, flags: u16, language: u16, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&32u32.to_le_bytes());
    put16(out, &[0xFFFF, kind, 0xFFFF, id]);
    out.extend_from_slice(&0u32.to_le_bytes());
    put16(out, &[flags, language]);
    out.extend_from_slice(&[0; 8]);
    out.extend_from_slice(data);
    out.resize(out.len().next_multiple_of(4), 0);
}

fn icon_resource(frames: &[(u32, Vec<u8>)]) -> Vec<u8> {
    const LANG_EN_US: u16 = 0x0409;
    let mut res = vec![];
    put_resource(&mut res, 0, 0, 0, 0, &[]);
    let mut group = vec![];
    put16(&mut group, &[0, 1, frames.len() as u16]);
    for (i, (size, png)) in frames.iter().enumerate() {
        let id = i as u16 + 1;
        put_resource(&mut res, RT_ICON, id, 0x1010, LANG_EN_US, png);
        put_dir_entry(&mut group, *size, png.len());
        put16(&mut group, &[id]);
    }
    put_resource(&mut res, RT_GROUP_ICON, 1, 0x1030, LANG_EN_US, &group);
    res
}
