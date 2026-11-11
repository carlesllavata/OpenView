// SPDX-License-Identifier: GPL-3.0-or-later
//! The app's icon, drawn in code: a globe on a dark rounded square, in the style of the other
//! Open3D apps. Used for the window and, through `examples/make_icon.rs`, for the executable.

/// How much of a pixel at (x, y) (in units of the icon's size, centre at 0) each shape covers.
fn shade(x: f32, y: f32) -> [f32; 4] {
    // Rounded square.
    let (half, corner) = (0.47, 0.11);
    let (dx, dy) = ((x.abs() - (half - corner)).max(0.0), (y.abs() - (half - corner)).max(0.0));
    if (dx * dx + dy * dy).sqrt() > corner {
        return [0.0; 4];
    }
    let background = [0.118 - 0.03 * y, 0.129 - 0.03 * y, 0.153 - 0.03 * y];
    // Distance to the outline of an ellipse, good enough for strokes.
    let ellipse = |rx: f32, ry: f32| {
        let f = ((x / rx).powi(2) + (y / ry).powi(2)).sqrt().max(1e-6);
        let gradient = ((x / (rx * rx)).powi(2) + (y / (ry * ry)).powi(2)).sqrt() / f;
        (f - 1.0).abs() / gradient.max(1e-6)
    };
    let radius = 0.31;
    let stroke = 0.026;
    let inside = (x * x + y * y).sqrt() <= radius + stroke;
    let white = [0.93, 0.94, 0.96];
    let accent = [0.20, 0.72, 0.66];
    if ellipse(radius, radius) <= stroke {
        return [white[0], white[1], white[2], 1.0];
    }
    if inside && ellipse(radius, radius * 0.36) <= stroke * 1.15 {
        return [accent[0], accent[1], accent[2], 1.0];
    }
    if inside && ellipse(radius * 0.42, radius) <= stroke * 0.8 {
        return [white[0], white[1], white[2], 1.0];
    }
    [background[0], background[1], background[2], 1.0]
}

/// The icon as RGBA pixels (straight alpha), `size` pixels on each side.
pub fn rgba(size: u32) -> Vec<u8> {
    const SAMPLES: u32 = 4;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut sum = [0f32; 4];
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let x = (px as f32 + (sx as f32 + 0.5) / SAMPLES as f32) / size as f32 - 0.5;
                    let y = (py as f32 + (sy as f32 + 0.5) / SAMPLES as f32) / size as f32 - 0.5;
                    let c = shade(x, y);
                    for k in 0..3 {
                        sum[k] += c[k] * c[3];
                    }
                    sum[3] += c[3];
                }
            }
            let alpha = sum[3] / (SAMPLES * SAMPLES) as f32;
            for k in 0..3 {
                let v = if sum[3] > 0.0 { sum[k] / sum[3] } else { 0.0 };
                out.push((v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
            }
            out.push((alpha * 255.0 + 0.5) as u8);
        }
    }
    out
}

/// The icon as a Windows `.ico` file with the usual sizes.
pub fn ico() -> Vec<u8> {
    let sizes = [16u32, 24, 32, 48, 64, 256];
    let images: Vec<Vec<u8>> = sizes.iter().map(|size| if *size >= 256 { png(*size) } else { dib(*size) }).collect();
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len();
    for (size, image) in sizes.iter().zip(&images) {
        let dimension = if *size >= 256 { 0 } else { *size as u8 };
        out.extend_from_slice(&[dimension, dimension, 0, 0, 1, 0, 32, 0]);
        out.extend_from_slice(&(image.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += image.len();
    }
    for image in &images {
        out.extend_from_slice(image);
    }
    out
}

fn png(size: u32) -> Vec<u8> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&rgba(size), size, size, image::ExtendedColorType::Rgba8)
        .expect("writing a PNG to memory cannot fail");
    out
}

/// A 32-bit bitmap the way icon files store the small sizes: bottom-up BGRA plus an empty mask.
fn dib(size: u32) -> Vec<u8> {
    let pixels = rgba(size);
    let mut out = Vec::new();
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0u8; 24]);
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            out.extend_from_slice(&[pixels[i + 2], pixels[i + 1], pixels[i], pixels[i + 3]]);
        }
    }
    let mask_row = size.div_ceil(32) * 4;
    out.extend(std::iter::repeat(0u8).take((mask_row * size) as usize));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_are_transparent_and_the_middle_is_not() {
        let pixels = rgba(64);
        assert_eq!(pixels.len(), 64 * 64 * 4);
        assert_eq!(pixels[3], 0, "the corner is not transparent");
        let centre = (32 * 64 + 32) * 4;
        assert_eq!(pixels[centre + 3], 255);
    }

    #[test]
    fn icon_file_lists_every_size() {
        let file = ico();
        assert_eq!(&file[..6], &[0, 0, 1, 0, 6, 0]);
        let last = 6 + 16 * 5;
        let length = u32::from_le_bytes(file[last + 8..last + 12].try_into().unwrap()) as usize;
        let offset = u32::from_le_bytes(file[last + 12..last + 16].try_into().unwrap()) as usize;
        assert_eq!(offset + length, file.len());
        assert_eq!(&file[offset..offset + 4], b"\x89PNG");
    }
}
