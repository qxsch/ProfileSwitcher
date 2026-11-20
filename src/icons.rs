use anyhow::{Context, Result, ensure};
use image::{
    ExtendedColorType,
    codecs::ico::{IcoEncoder, IcoFrame},
};
use std::{io::Write, sync::LazyLock};

pub const COLORS: [(&str, &str); 6] = [
    ("Pine", "#3d6652"),
    ("Iris", "#6d5994"),
    ("Clay", "#935b37"),
    ("Slate", "#486781"),
    ("Teal", "#326e70"),
    ("Olive", "#6b6937"),
];
pub static SYMBOLS: LazyLock<Vec<(&str, &str)>> = LazyLock::new(|| {
    include_str!("../assets/symbols.tsv")
        .lines()
        .map(|line| line.split_once('\t').expect("icon catalog entry"))
        .collect()
});

pub fn valid(symbol: usize, color: usize) -> Result<()> {
    ensure!(
        symbol < SYMBOLS.len() && color < COLORS.len(),
        "Unknown icon or color"
    );
    Ok(())
}

fn pixels(symbol: usize, color: usize, size: u32) -> Result<resvg::tiny_skia::Pixmap> {
    valid(symbol, color)?;
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="48" height="48"><rect x="1" y="1" width="46" height="46" rx="11" fill="{}"/><path transform="translate(8 8)" d="{}" fill="none" stroke="#fff9ee" stroke-width="2.15" stroke-linecap="round" stroke-linejoin="round"/></svg>"##,
        COLORS[color].1, SYMBOLS[symbol].1
    );
    let tree = resvg::usvg::Tree::from_str(&svg, &Default::default())?;
    let mut surface = resvg::tiny_skia::Pixmap::new(size, size).context("icon dimensions")?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(size as f32 / 48., size as f32 / 48.),
        &mut surface.as_mut(),
    );
    Ok(surface)
}

pub fn preview(symbol: usize, color: usize) -> Result<slint::Image> {
    let px = pixels(symbol, color, 64)?;
    Ok(slint::Image::from_rgba8_premultiplied(
        slint::SharedPixelBuffer::clone_from_slice(px.data(), 64, 64),
    ))
}

pub fn write_ico(writer: impl Write, symbol: usize, color: usize) -> Result<()> {
    let mut frames = vec![];
    for size in [16, 24, 32, 48, 64, 128, 256] {
        let png = pixels(symbol, color, size)?.encode_png()?;
        frames.push(IcoFrame::with_encoded(
            png,
            size,
            size,
            ExtendedColorType::Rgba8,
        )?);
    }
    IcoEncoder::new(writer).encode_images(&frames)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_combination_has_distinct_visible_artwork() {
        let mut images = std::collections::HashSet::new();
        for symbol in 0..SYMBOLS.len() {
            for color in 0..COLORS.len() {
                let p = pixels(symbol, color, 48).unwrap();
                assert!(
                    p.data()
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .filter(|v| v[0] > 225 && v[1] > 225 && v[3] > 200)
                        .count()
                        > 12,
                    "{}",
                    SYMBOLS[symbol].0
                );
                assert!(images.insert(p.take()), "duplicate artwork");
            }
        }
        assert_eq!(images.len(), 600);
    }
}
