//! Export the runtime PNG and multi-resolution Windows ICO from a rendered SVG.
//! Usage: cargo run --example export_app_icon -- input.png assets/icons
use std::path::PathBuf;

use image::codecs::ico::{IcoEncoder, IcoFrame};
use image::imageops::FilterType;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let input = PathBuf::from(args.next().ok_or("Expected an input PNG")?);
    let output = PathBuf::from(args.next().ok_or("Expected an output directory")?);
    let source = image::open(input)?.into_rgba8();
    if source.width() != source.height() || source.width() < 256 {
        return Err("Expected a square PNG of at least 256 pixels".into());
    }
    std::fs::create_dir_all(&output)?;
    let runtime = image::imageops::resize(&source, 256, 256, FilterType::Lanczos3);
    runtime.save(output.join("archive-sift.png"))?;
    let mut frames = Vec::new();
    for size in [16, 24, 32, 48, 64, 128, 256] {
        let pixels = image::imageops::resize(&source, size, size, FilterType::Lanczos3);
        frames.push(IcoFrame::as_png(
            &pixels,
            size,
            size,
            image::ExtendedColorType::Rgba8,
        )?);
    }
    IcoEncoder::new(std::fs::File::create(output.join("archive-sift.ico"))?)
        .encode_images(&frames)?;
    Ok(())
}
