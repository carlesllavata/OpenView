// SPDX-License-Identifier: GPL-3.0-or-later
//! Turns an ordinary 360 image into an HDRI that can light a scene.
//!
//! A JPG does not keep the real intensity of the sun or the sky, so it is rebuilt (no AI): the
//! image goes to linear values, the brightest parts (sky, clouds, reflections) get their range
//! back, and a sun disc is added where the sun is, with the strength of a real day.

use std::f32::consts::{PI, TAU};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use image::{GrayImage, RgbImage};
use rayon::prelude::*;

/// Width of the copy used to find the sun, measure the sky and draw the preview.
pub const ANALYSIS_WIDTH: u32 = 1024;
/// How many times brighter a blown-out pixel becomes.
const HIGHLIGHT_GAIN: f32 = 4.0;
/// Width of the glow around the sun, in degrees.
const GLOW_DEG: f32 = 5.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HdrFormat {
    Exr,
    Hdr,
}

impl HdrFormat {
    pub fn extension(self) -> &'static str {
        match self {
            HdrFormat::Exr => "exr",
            HdrFormat::Hdr => "hdr",
        }
    }

    pub fn parse(text: &str) -> Option<HdrFormat> {
        match text.to_ascii_lowercase().as_str() {
            "exr" => Some(HdrFormat::Exr),
            "hdr" => Some(HdrFormat::Hdr),
            _ => None,
        }
    }
}

/// Where the sun is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sun {
    /// Find it in the image.
    Auto,
    /// No sun: an overcast day.
    None,
    /// At this position of the image: x and y from 0 to 1 (left to right, top to bottom).
    At(f32, f32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    /// The light the sun gives compared with the light of the rest of the sky. Clear day 4 to 8,
    /// haze 1 to 3, overcast 0.
    pub sun_strength: f32,
    /// Diameter of the sun disc in degrees. Smaller means sharper shadows.
    pub sun_size_deg: f32,
    pub sun: Sun,
}

impl Default for Params {
    fn default() -> Self {
        Params { sun_strength: 6.0, sun_size_deg: 1.5, sun: Sun::Auto }
    }
}

/// An image ready to be converted.
pub struct Source {
    pub rgb: RgbImage,
    /// Transparency of the source, when it has any.
    pub alpha: Option<GrayImage>,
}

#[derive(Debug)]
pub enum Error {
    Image(String),
    Io(std::io::Error),
    /// The image is not a 2:1 panorama.
    NotSpherical(u32, u32),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Image(what) => write!(f, "image error: {what}"),
            Error::Io(e) => write!(f, "file error: {e}"),
            Error::NotSpherical(w, h) => write!(
                f,
                "the image is {w} x {h}: an HDRI needs a full 360 panorama, twice as wide as it is tall"
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// Opens a panorama from disk (JPG, PNG or TIFF), keeping its transparency.
pub fn load(path: &Path) -> Result<Source, Error> {
    let mut reader = image::ImageReader::open(path)?.with_guessed_format()?;
    reader.no_limits();
    let decoded = reader.decode().map_err(|e| Error::Image(e.to_string()))?;
    let alpha = if decoded.color().has_alpha() {
        let rgba = decoded.to_rgba8();
        let alpha = GrayImage::from_fn(rgba.width(), rgba.height(), |x, y| image::Luma([rgba.get_pixel(x, y)[3]]));
        alpha.as_raw().iter().any(|a| *a < 255).then_some(alpha)
    } else {
        None
    };
    let rgb = decoded.into_rgb8();
    let (w, h) = rgb.dimensions();
    // A pixel of tolerance for images that were resized by hand.
    if w.abs_diff(h * 2) > 2 {
        return Err(Error::NotSpherical(w, h));
    }
    Ok(Source { rgb, alpha })
}

fn srgb_to_linear_table() -> [f32; 256] {
    let mut table = [0f32; 256];
    for (i, entry) in table.iter_mut().enumerate() {
        let v = i as f32 / 255.0;
        *entry = if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
    }
    table
}

fn linear_to_srgb(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let s = if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
    (s * 255.0 + 0.5) as u8
}

fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Direction (x right, y up, z to the centre of the image) of a point of the image given from 0
/// to 1.
fn direction(u: f32, v: f32) -> [f32; 3] {
    let lon = (u - 0.5) * TAU;
    let lat = (0.5 - v) * PI;
    [lat.cos() * lon.sin(), lat.sin(), lat.cos() * lon.cos()]
}

/// The copy of the image the analysis and the preview work on.
pub fn analysis_copy(rgb: &RgbImage) -> RgbImage {
    if rgb.width() <= ANALYSIS_WIDTH {
        return rgb.clone();
    }
    image::imageops::resize(rgb, ANALYSIS_WIDTH, ANALYSIS_WIDTH / 2, image::imageops::FilterType::Triangle)
}

/// Finds the sun: the centre of the largest blown-out patch of the sky. Returns its position from
/// 0 to 1, or `None` when the sky has no such patch (overcast) or is blown out all over.
pub fn detect_sun(small: &RgbImage) -> Option<(f32, f32)> {
    let (w, h) = (small.width() as usize, small.height() as usize);
    let table = srgb_to_linear_table();
    // Only the sky: from the top down to a little below the horizon.
    let rows = (h as f32 * 0.52) as usize;
    let mut weight = vec![0f32; w * rows];
    for y in 0..rows {
        for x in 0..w {
            let p = small.get_pixel(x as u32, y as u32);
            let darkest = table[p[0].min(p[1]).min(p[2]) as usize];
            weight[y * w + x] = smoothstep(0.8, 1.0, darkest);
        }
    }
    let blown = weight.iter().filter(|v| **v > 0.5).count() as f32 / weight.len() as f32;
    if blown > 0.4 {
        return None;
    }
    // Blur over about 3 degrees so that a big patch beats scattered white pixels.
    let radius = (w as f32 * 3.0 / 360.0).ceil() as usize;
    let blurred = box_blur(&box_blur(&weight, w, rows, radius), w, rows, radius);
    let (mut best, mut best_at) = (0f32, 0usize);
    for (i, v) in blurred.iter().enumerate() {
        if *v > best {
            best = *v;
            best_at = i;
        }
    }
    if best < 0.12 {
        return None;
    }
    // Refine: the centre of mass of the blown-out pixels around the peak.
    let (px, py) = ((best_at % w) as isize, (best_at / w) as isize);
    let reach = (radius * 3) as isize;
    let (mut sx, mut sy, mut total) = (0f32, 0f32, 0f32);
    for dy in -reach..=reach {
        let y = py + dy;
        if y < 0 || y >= rows as isize {
            continue;
        }
        for dx in -reach..=reach {
            let x = (px + dx).rem_euclid(w as isize);
            let v = weight[y as usize * w + x as usize];
            sx += v * dx as f32;
            sy += v * dy as f32;
            total += v;
        }
    }
    if total <= 0.0 {
        return None;
    }
    let x = (px as f32 + sx / total + 0.5).rem_euclid(w as f32);
    let y = py as f32 + sy / total + 0.5;
    Some((x / w as f32, y / h as f32))
}

/// Box blur that wraps around horizontally.
fn box_blur(values: &[f32], w: usize, h: usize, radius: usize) -> Vec<f32> {
    let span = (radius * 2 + 1) as f32;
    let mut horizontal = vec![0f32; values.len()];
    for y in 0..h {
        let row = &values[y * w..(y + 1) * w];
        let mut sum: f32 = (0..radius * 2 + 1).map(|i| row[(i + w - radius % w) % w]).sum();
        for x in 0..w {
            horizontal[y * w + x] = sum / span;
            sum += row[(x + radius + 1) % w] - row[(x + w - radius % w) % w];
        }
    }
    let mut out = vec![0f32; values.len()];
    for x in 0..w {
        for y in 0..h {
            let (from, to) = (y.saturating_sub(radius), (y + radius).min(h - 1));
            let mut sum = 0f32;
            for yy in from..=to {
                sum += horizontal[yy * w + x];
            }
            out[y * w + x] = sum / (to - from + 1) as f32;
        }
    }
    out
}

/// Everything needed to compute the HDR value of any pixel.
pub struct Model {
    table: [f32; 256],
    /// Where the sun is, from 0 to 1, when there is one.
    pub sun: Option<(f32, f32)>,
    sun_dir: [f32; 3],
    /// Radiance of the sun disc.
    sun_radiance: [f32; 3],
    cos_inner: f32,
    cos_outer: f32,
    glow: f32,
}

impl Model {
    /// Measures the sky on the analysis copy and prepares the conversion for an image `out_width`
    /// pixels wide.
    pub fn new(small: &RgbImage, params: &Params, out_width: u32) -> Model {
        let sun = match params.sun {
            Sun::None => None,
            Sun::At(x, y) => Some((x.rem_euclid(1.0), y.clamp(0.0, 1.0))),
            Sun::Auto => detect_sun(small),
        };
        let strength = params.sun_strength.max(0.0);
        let sun = if strength > 0.0 { sun } else { None };
        let mut model = Model {
            table: srgb_to_linear_table(),
            sun,
            sun_dir: [0.0, 1.0, 0.0],
            sun_radiance: [0.0; 3],
            cos_inner: 2.0,
            cos_outer: 2.0,
            glow: 0.0,
        };
        let Some((sx, sy)) = sun else { return model };
        model.sun_dir = direction(sx, sy);
        // The glow is part of the sky, so it is in place before the sky is measured.
        model.glow = 1.5 * (strength / 6.0).min(2.0);

        // Light the sky throws on the ground: every pixel of the upper half, weighted by its
        // height and by the patch of sky it covers.
        let (w, h) = (small.width() as usize, small.height() as usize);
        let mut sky = 0f64;
        for y in 0..h / 2 {
            let v = (y as f32 + 0.5) / h as f32;
            let lat = (0.5 - v) * PI;
            let solid = (lat.cos() * (PI / h as f32) * (TAU / w as f32)) as f64;
            for x in 0..w {
                let p = small.get_pixel(x as u32, y as u32).0;
                let dir = direction((x as f32 + 0.5) / w as f32, v);
                sky += luminance(model.shade(p, dir)) as f64 * lat.sin() as f64 * solid;
            }
        }
        // A disc smaller than a few pixels would fall between them and lose its light.
        let smallest = 2.5 * 360.0 / out_width.max(64) as f32;
        let radius = (params.sun_size_deg.clamp(0.1, 20.0).max(smallest) / 2.0).to_radians();
        let soft = radius * 1.15;
        model.cos_inner = (radius * 0.85).cos();
        model.cos_outer = soft.cos();
        let solid_angle = TAU * (1.0 - radius.cos());
        // A sun on the horizon would need an absurd radiance to light the ground: limit it.
        let height = model.sun_dir[1].max(0.2);
        let radiance = strength * sky as f32 / (solid_angle * height);
        // Low suns are warmer.
        let warmth = smoothstep(0.0, 0.6, model.sun_dir[1]);
        let colour = [1.0, 0.62 + 0.34 * warmth, 0.32 + 0.56 * warmth];
        let scale = radiance / luminance(colour);
        model.sun_radiance = [colour[0] * scale, colour[1] * scale, colour[2] * scale];
        model
    }

    /// The HDR value of a pixel of the source that lies in direction `dir`.
    pub fn shade(&self, pixel: [u8; 3], dir: [f32; 3]) -> [f32; 3] {
        let linear = [self.table[pixel[0] as usize], self.table[pixel[1] as usize], self.table[pixel[2] as usize]];
        let bright = smoothstep(0.5, 1.0, luminance(linear));
        // The sky gets its whole range back; white walls and pavements below the horizon were
        // never that bright, so they get a quarter of it.
        let sky = 0.25 + 0.75 * smoothstep(-0.17, 0.26, dir[1]);
        let mut gain = 1.0 + (HIGHLIGHT_GAIN - 1.0) * bright * bright * sky;
        let mut out = linear;
        if self.sun.is_some() {
            let cos = (dir[0] * self.sun_dir[0] + dir[1] * self.sun_dir[1] + dir[2] * self.sun_dir[2]).clamp(-1.0, 1.0);
            if cos > 0.9 {
                let angle = cos.acos().to_degrees();
                gain *= 1.0 + self.glow * bright * (-(angle / GLOW_DEG) * (angle / GLOW_DEG)).exp();
            }
            if cos > self.cos_outer {
                let disc = smoothstep(self.cos_outer, self.cos_inner, cos);
                for k in 0..3 {
                    out[k] = out[k] * gain + self.sun_radiance[k] * disc;
                }
                return out;
            }
        }
        [out[0] * gain, out[1] * gain, out[2] * gain]
    }

    /// The HDRI as it would look photographed with `ev` stops of exposure compensation.
    pub fn preview(&self, small: &RgbImage, ev: f32) -> RgbImage {
        let (w, h) = small.dimensions();
        let exposure = 2f32.powf(ev);
        let mut out = vec![0u8; (w * h * 3) as usize];
        out.par_chunks_mut(w as usize * 3).enumerate().for_each(|(y, row)| {
            let v = (y as f32 + 0.5) / h as f32;
            for x in 0..w as usize {
                let p = small.get_pixel(x as u32, y as u32).0;
                let c = self.shade(p, direction((x as f32 + 0.5) / w as f32, v));
                for k in 0..3 {
                    row[x * 3 + k] = linear_to_srgb(c[k] * exposure);
                }
            }
        });
        RgbImage::from_raw(w, h, out).expect("buffer has the right size")
    }
}

/// The source at the width asked for (never larger than the original).
fn sized(source: &Source, width: Option<u32>) -> (std::borrow::Cow<'_, RgbImage>, Option<std::borrow::Cow<'_, GrayImage>>) {
    use std::borrow::Cow;
    let (w, h) = source.rgb.dimensions();
    match width {
        Some(target) if target < w && target >= 16 => {
            let filter = image::imageops::FilterType::CatmullRom;
            let target_h = (target as u64 * h as u64 / w as u64).max(1) as u32;
            let rgb = image::imageops::resize(&source.rgb, target, target_h, filter);
            let alpha = source.alpha.as_ref().map(|a| Cow::Owned(image::imageops::resize(a, target, target_h, filter)));
            (Cow::Owned(rgb), alpha)
        }
        _ => (Cow::Borrowed(&source.rgb), source.alpha.as_ref().map(Cow::Borrowed)),
    }
}

/// `panorama.jpg` becomes `panorama_hdri.exr` next to it.
pub fn output_path(source: &Path, format: HdrFormat) -> PathBuf {
    let stem = source.file_stem().and_then(|s| s.to_str()).unwrap_or("panorama");
    source.with_file_name(format!("{stem}_hdri.{}", format.extension()))
}

/// Converts and writes the HDRI. Returns the size written.
pub fn write(source: &Source, params: &Params, width: Option<u32>, format: HdrFormat, path: &Path) -> Result<(u32, u32), Error> {
    let small = analysis_copy(&source.rgb);
    let (rgb, alpha) = sized(source, width);
    let (w, h) = rgb.dimensions();
    let model = Model::new(&small, params, w);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let partial = path.with_extension(format!("{}.part", format.extension()));
    let result = match format {
        HdrFormat::Exr => write_exr(&model, &rgb, alpha.as_deref(), &partial),
        HdrFormat::Hdr => write_radiance(&model, &rgb, &partial),
    };
    match result {
        Ok(()) => {
            let _ = fs::remove_file(path);
            fs::rename(&partial, path)?;
            Ok((w, h))
        }
        Err(e) => {
            let _ = fs::remove_file(&partial);
            Err(e)
        }
    }
}

/// The direction of every column and row, so that pixels only need a few multiplications.
struct Directions {
    columns: Vec<(f32, f32)>,
    rows: Vec<(f32, f32)>,
}

impl Directions {
    fn new(w: u32, h: u32) -> Self {
        Directions {
            columns: (0..w).map(|x| (((x as f32 + 0.5) / w as f32 - 0.5) * TAU).sin_cos()).collect(),
            rows: (0..h).map(|y| ((0.5 - (y as f32 + 0.5) / h as f32) * PI).sin_cos()).collect(),
        }
    }

    fn at(&self, x: usize, y: usize) -> [f32; 3] {
        let (sin_lon, cos_lon) = self.columns[x];
        let (sin_lat, cos_lat) = self.rows[y];
        [cos_lat * sin_lon, sin_lat, cos_lat * cos_lon]
    }
}

fn write_exr(model: &Model, rgb: &RgbImage, alpha: Option<&GrayImage>, path: &Path) -> Result<(), crate::hdri::Error> {
    use exr::prelude::*;
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    let directions = Directions::new(rgb.width(), rgb.height());
    let data = rgb.as_raw();
    let value = |x: usize, y: usize| {
        let i = (y * w + x) * 3;
        model.shade([data[i], data[i + 1], data[i + 2]], directions.at(x, y))
    };
    let attributes = LayerAttributes::default();
    let encoding = Encoding::SMALL_LOSSLESS;
    let result = match alpha {
        Some(alpha) => {
            let alpha = alpha.as_raw();
            let channels = SpecificChannels::rgba(|Vec2(x, y)| {
                let c = value(x, y);
                (c[0], c[1], c[2], alpha[y * w + x] as f32 / 255.0)
            });
            Image::from_layer(Layer::new((w, h), attributes, encoding, channels)).write().to_file(path)
        }
        None => {
            let channels = SpecificChannels::rgb(|Vec2(x, y)| {
                let c = value(x, y);
                (c[0], c[1], c[2])
            });
            Image::from_layer(Layer::new((w, h), attributes, encoding, channels)).write().to_file(path)
        }
    };
    result.map_err(|e| crate::hdri::Error::Image(e.to_string()))
}

fn to_rgbe(c: [f32; 3]) -> [u8; 4] {
    let peak = c[0].max(c[1]).max(c[2]);
    if peak < 1e-32 {
        return [0, 0, 0, 0];
    }
    let exponent = peak.log2().floor() as i32 + 1;
    let scale = 2f32.powi(-exponent) * 256.0;
    [
        (c[0].max(0.0) * scale).min(255.0) as u8,
        (c[1].max(0.0) * scale).min(255.0) as u8,
        (c[2].max(0.0) * scale).min(255.0) as u8,
        (exponent + 128).clamp(0, 255) as u8,
    ]
}

/// Run-length encodes one channel of a scanline the way Radiance files do.
fn rle(values: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < values.len() {
        let mut run = 1;
        while i + run < values.len() && run < 127 && values[i + run] == values[i] {
            run += 1;
        }
        if run >= 3 {
            out.push(128 + run as u8);
            out.push(values[i]);
            i += run;
            continue;
        }
        // Literal bytes, up to the next run of three.
        let start = i;
        while i < values.len() && i - start < 128 {
            if i + 2 < values.len() && values[i] == values[i + 1] && values[i] == values[i + 2] {
                break;
            }
            i += 1;
        }
        out.push((i - start) as u8);
        out.extend_from_slice(&values[start..i]);
    }
}

fn write_radiance(model: &Model, rgb: &RgbImage, path: &Path) -> Result<(), Error> {
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    let directions = Directions::new(rgb.width(), rgb.height());
    let data = rgb.as_raw();
    let mut file = BufWriter::with_capacity(1 << 20, fs::File::create(path)?);
    write!(file, "#?RADIANCE\nSOFTWARE=OpenView {}\nFORMAT=32-bit_rle_rgbe\n\n-Y {h} +X {w}\n", crate::VERSION)?;
    // Scanlines are independent: encode a batch at a time in parallel, write them in order.
    for batch in (0..h).collect::<Vec<_>>().chunks(256) {
        let lines: Vec<Vec<u8>> = batch
            .par_iter()
            .map(|&y| {
                let pixels: Vec<[u8; 4]> = (0..w)
                    .map(|x| {
                        let i = (y * w + x) * 3;
                        to_rgbe(model.shade([data[i], data[i + 1], data[i + 2]], directions.at(x, y)))
                    })
                    .collect();
                let mut line = Vec::with_capacity(w * 2);
                if (8..=0x7fff).contains(&w) {
                    line.extend_from_slice(&[2, 2, (w >> 8) as u8, (w & 0xff) as u8]);
                    let mut channel = vec![0u8; w];
                    for k in 0..4 {
                        for (dst, p) in channel.iter_mut().zip(&pixels) {
                            *dst = p[k];
                        }
                        rle(&channel, &mut line);
                    }
                } else {
                    for p in &pixels {
                        line.extend_from_slice(p);
                    }
                }
                line
            })
            .collect();
        for line in lines {
            file.write_all(&line)?;
        }
    }
    file.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    /// A blue sky over a grey ground with a white sun at a known place.
    fn sunny(w: u32, sun_u: f32, sun_v: f32) -> RgbImage {
        let h = w / 2;
        let sun = direction(sun_u, sun_v);
        RgbImage::from_fn(w, h, |x, y| {
            let d = direction((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32);
            let cos = d[0] * sun[0] + d[1] * sun[1] + d[2] * sun[2];
            if cos > 4f32.to_radians().cos() {
                Rgb([255, 255, 255])
            } else if y < h / 2 {
                Rgb([110, 150, 220])
            } else {
                Rgb([90, 90, 90])
            }
        })
    }

    #[test]
    fn the_sun_is_found_where_it_is() {
        let image = sunny(1024, 0.7, 0.25);
        let (x, y) = detect_sun(&image).expect("sun found");
        assert!((x - 0.7).abs() < 0.01 && (y - 0.25).abs() < 0.01, "found at {x},{y}");
    }

    #[test]
    fn the_sun_is_found_across_the_seam() {
        let image = sunny(1024, 0.001, 0.3);
        let (x, y) = detect_sun(&image).expect("sun found");
        let distance = (x - 0.001).abs().min(1.0 - (x - 0.001).abs());
        assert!(distance < 0.01 && (y - 0.3).abs() < 0.01, "found at {x},{y}");
    }

    #[test]
    fn an_overcast_sky_has_no_sun() {
        let grey = RgbImage::from_pixel(512, 256, Rgb([170, 175, 180]));
        assert_eq!(detect_sun(&grey), None);
        let white = RgbImage::from_fn(512, 256, |_, y| if y < 128 { Rgb([255, 255, 255]) } else { Rgb([80, 80, 80]) });
        assert_eq!(detect_sun(&white), None);
    }

    #[test]
    fn the_sun_gives_the_light_that_was_asked_for() {
        let image = sunny(1024, 0.5, 0.2);
        let params = Params { sun_strength: 5.0, sun_size_deg: 3.0, sun: Sun::Auto };
        let model = Model::new(&image, &params, 1024);
        assert!(model.sun.is_some());
        // Measure the light on the ground from the disc and from the rest of the sky.
        let (w, h) = (1024usize, 512usize);
        let no_sun = Model::new(&image, &Params { sun: Sun::None, ..params }, 1024);
        let (mut with, mut without) = (0f64, 0f64);
        for y in 0..h / 2 {
            let v = (y as f32 + 0.5) / h as f32;
            let lat = (0.5 - v) * PI;
            let weight = (lat.sin() * lat.cos()) as f64;
            for x in 0..w {
                let p = image.get_pixel(x as u32, y as u32).0;
                let dir = direction((x as f32 + 0.5) / w as f32, v);
                with += luminance(model.shade(p, dir)) as f64 * weight;
                without += luminance(no_sun.shade(p, dir)) as f64 * weight;
            }
        }
        let ratio = (with - without) / without;
        assert!((3.5..7.0).contains(&ratio), "the sun gives {ratio} times the sky");
    }

    #[test]
    fn strength_zero_or_no_sun_adds_nothing() {
        let image = sunny(512, 0.5, 0.2);
        let off = Model::new(&image, &Params { sun_strength: 0.0, ..Params::default() }, 512);
        assert!(off.sun.is_none());
        let peak = off.shade([255, 255, 255], direction(0.5, 0.2));
        assert!((peak[0] - HIGHLIGHT_GAIN).abs() < 1e-3, "{peak:?}");
        // Dark and mid tones keep their linear value.
        let mid = off.shade([128, 128, 128], direction(0.1, 0.6));
        assert!((mid[0] - 0.2158).abs() < 1e-3, "{mid:?}");
    }

    #[test]
    fn a_clicked_sun_is_used() {
        let image = sunny(512, 0.5, 0.2);
        let model = Model::new(&image, &Params { sun: Sun::At(0.25, 0.3), ..Params::default() }, 512);
        assert_eq!(model.sun, Some((0.25, 0.3)));
        let there = model.shade([110, 150, 220], direction(0.25, 0.3));
        assert!(there[0] > 100.0, "no disc at the clicked place: {there:?}");
    }

    #[test]
    fn the_preview_darkens_with_exposure() {
        let image = sunny(256, 0.5, 0.2);
        let model = Model::new(&image, &Params::default(), 256);
        let normal = model.preview(&image, 0.0);
        let dark = model.preview(&image, -8.0);
        assert_eq!(normal.dimensions(), (256, 128));
        // At -8 stops only the sun is left.
        assert!(dark.get_pixel(128, 25)[0] > 200, "{:?}", dark.get_pixel(128, 25));
        assert!(dark.get_pixel(10, 100)[0] < 5 && dark.get_pixel(10, 10)[2] < 30);
        assert!(normal.get_pixel(10, 100)[0] > 80);
    }

    #[test]
    fn rgbe_round_trips() {
        for value in [[0.0, 0.0, 0.0], [1.0, 0.5, 0.25], [50000.0, 30000.0, 10.0], [0.001, 0.002, 0.003]] {
            let [r, g, b, e] = to_rgbe(value);
            let scale = if e == 0 { 0.0 } else { 2f32.powi(e as i32 - 128 - 8) };
            let back = [r as f32 * scale, g as f32 * scale, b as f32 * scale];
            let peak = value[0].max(value[1]).max(value[2]);
            for k in 0..3 {
                assert!((back[k] - value[k]).abs() <= peak / 100.0 + 1e-9, "{value:?} came back as {back:?}");
            }
        }
    }

    #[test]
    fn run_lengths_decode_to_the_same_bytes() {
        let mut values = vec![7u8; 300];
        values.extend_from_slice(&[1, 2, 3, 3, 4, 5, 5, 5, 5, 6]);
        values.extend((0..200).map(|i| (i % 251) as u8));
        let mut packed = Vec::new();
        rle(&values, &mut packed);
        let mut unpacked = Vec::new();
        let mut i = 0;
        while i < packed.len() {
            let count = packed[i] as usize;
            if count > 128 {
                unpacked.extend(std::iter::repeat(packed[i + 1]).take(count - 128));
                i += 2;
            } else {
                assert!(count > 0);
                unpacked.extend_from_slice(&packed[i + 1..i + 1 + count]);
                i += 1 + count;
            }
        }
        assert_eq!(unpacked, values);
        assert!(packed.len() < values.len());
    }

    #[test]
    fn files_are_written_in_both_formats() {
        let dir = std::env::temp_dir().join(format!("openview-hdri-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let source = Source { rgb: sunny(256, 0.5, 0.2), alpha: None };
        let exr_path = dir.join("p_hdri.exr");
        assert_eq!(write(&source, &Params::default(), Some(128), HdrFormat::Exr, &exr_path).unwrap(), (128, 64));
        let back = exr::prelude::read_first_rgba_layer_from_file(
            &exr_path,
            |resolution, _| vec![vec![[0f32; 4]; resolution.width()]; resolution.height()],
            |rows: &mut Vec<Vec<[f32; 4]>>, position, (r, g, b, a): (f32, f32, f32, f32)| {
                rows[position.y()][position.x()] = [r, g, b, a];
            },
        )
        .expect("the EXR can be read");
        let rows = &back.layer_data.channel_data.pixels;
        assert_eq!((rows[0].len(), rows.len()), (128, 64));
        let peak = rows.iter().flatten().map(|p| p[0]).fold(0f32, f32::max);
        assert!(peak > 100.0, "the sun is missing from the file: {peak}");

        let hdr_path = dir.join("p_hdri.hdr");
        assert_eq!(write(&source, &Params::default(), None, HdrFormat::Hdr, &hdr_path).unwrap(), (256, 128));
        let bytes = fs::read(&hdr_path).unwrap();
        assert!(bytes.starts_with(b"#?RADIANCE\n"));
        assert!(bytes.windows(15).any(|w| w == b"-Y 128 +X 256\n\x02"));
        assert_eq!(output_path(Path::new("a/b/pano.jpg"), HdrFormat::Exr), Path::new("a/b/pano_hdri.exr"));
        let _ = fs::remove_dir_all(&dir);
    }
}
