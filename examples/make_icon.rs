// SPDX-License-Identifier: GPL-3.0-or-later
//! Development tool: writes `assets/icon.ico` (used for the executable) and `assets/icon.png`
//! from the drawing in `src/icon.rs`. Run it from the repository root after changing the icon:
//!
//! `cargo run --example make_icon`

use image::ImageEncoder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all("assets")?;
    std::fs::write("assets/icon.ico", openview::icon::ico())?;
    let file = std::fs::File::create("assets/icon.png")?;
    image::codecs::png::PngEncoder::new(file).write_image(
        &openview::icon::rgba(256),
        256,
        256,
        image::ExtendedColorType::Rgba8,
    )?;
    println!("wrote assets/icon.ico and assets/icon.png");
    Ok(())
}
