// SPDX-License-Identifier: GPL-3.0-or-later
//! Turns an ordinary 360 image (JPG, PNG or TIFF) into an HDRI (.exr or .hdr) that can light a
//! scene.
//!
//! An 8-bit image does not keep the real intensity of the sun or the sky (they come out blown to
//! white), so it is rebuilt in a believable way, without AI:
//!
//! 1. The image goes to linear values, the way a renderer works.
//! 2. The very bright areas (sky, lit clouds, reflections) get their range back with a smooth curve.
//! 3. The sun (the brightest blown-out patch above the horizon) is found and given the strength
//!    that makes its light N times the light of the rest of the sky (5 = a clear day), so shadows
//!    have the hardness and contrast of a real day.
//!
//! The method and its numbers are those of the original OpenView by Daniel Castellanos (MIT, see
//! NOTICE). The EXR and Radiance writers are this program's own.

use std::f32::consts::{PI, TAU};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use image::{GrayImage, RgbImage};
use rayon::prelude::*;

/// Size of the copy used to find the sun, measure the sky and draw the preview.
pub const ANALYSIS_SIZE: (u32, u32) = (1024, 512);
const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];
/// Values above this do not fit in a half-precision float.
const MAX_VALUE: f32 = 65000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    /// The light the sun adds compared with the light of everything else. Clear day 4 to 8,
    /// haze 1 to 3, overcast 0 (no sun).
    pub sun_ratio: f32,
    /// Radius of the sun disc in degrees. Smaller means sharper shadows.
    pub sun_size: f32,
    /// How much the very bright areas are raised (1 = not at all).
    pub highlights: f32,
    /// Overall exposure in stops.
    pub exposure: f32,
    /// Where the sun is, from 0 to 1 across and down the image; `None` finds it.
    pub sun_pos: Option<(f32, f32)>,
}

impl Default for Params {
    fn default() -> Self {
        Params { sun_ratio: 5.0, sun_size: 1.0, highlights: 3.0, exposure: 0.0, sun_pos: None }
    }
}

impl Params {
    /// The same parameters with every number inside the range the interface offers.
    pub fn clamped(self) -> Params {
        let finite = |v: f32, default: f32| if v.is_finite() { v } else { default };
        let d = Params::default();
        Params {
            sun_ratio: finite(self.sun_ratio, d.sun_ratio).clamp(0.0, 20.0),
            sun_size: finite(self.sun_size, d.sun_size).clamp(0.3, 5.0),
            highlights: finite(self.highlights, d.highlights).clamp(1.0, 10.0),
            exposure: finite(self.exposure, d.exposure).clamp(-5.0, 5.0),
            sun_pos: self
                .sun_pos
                .filter(|(u, v)| u.is_finite() && v.is_finite())
                .map(|(u, v)| (u.clamp(0.0, 1.0), v.clamp(0.0, 1.0))),
        }
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
    TooSmall,
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", crate::text::Lang::En.hdri(self))
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

fn check_size(w: u32, h: u32) -> Result<(), Error> {
    if w < 64 || h < 32 {
        return Err(Error::TooSmall);
    }
    if (w as f32 / h as f32 - 2.0).abs() > 0.05 {
        return Err(Error::NotSpherical(w, h));
    }
    Ok(())
}

/// The size of a panorama on disk, without decoding it.
pub fn dimensions(path: &Path) -> Result<(u32, u32), Error> {
    let (w, h) = image::ImageReader::open(path)?
        .with_guessed_format()?
        .into_dimensions()
        .map_err(|e| Error::Image(e.to_string()))?;
    check_size(w, h)?;
    Ok((w, h))
}

/// Opens a panorama from disk (JPG, PNG or TIFF), keeping its transparency.
pub fn load(path: &Path) -> Result<Source, Error> {
    let mut reader = image::ImageReader::open(path)?.with_guessed_format()?;
    reader.no_limits();
    let decoded = reader.decode().map_err(|e| match e {
        image::ImageError::IoError(io) => Error::Io(io),
        other => Error::Image(other.to_string()),
    })?;
    check_size(decoded.width(), decoded.height())?;
    let alpha = if decoded.color().has_alpha() {
        let rgba = decoded.to_rgba8();
        let alpha = GrayImage::from_fn(rgba.width(), rgba.height(), |x, y| image::Luma([rgba.get_pixel(x, y)[3]]));
        alpha.as_raw().iter().any(|a| *a < 255).then_some(alpha)
    } else {
        None
    };
    Ok(Source { rgb: decoded.into_rgb8(), alpha })
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
    LUMA[0] * c[0] + LUMA[1] * c[1] + LUMA[2] * c[2]
}

/// Hermite step from `edge0` to `edge1`; it also works with `edge0 > edge1` (a falling step).
fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Direction (x right, y up, z to the centre of the image) of a point of the image given from 0
/// to 1.
pub fn direction(u: f32, v: f32) -> [f32; 3] {
    let lon = (u - 0.5) * TAU;
    let lat = (0.5 - v) * PI;
    [lat.cos() * lon.sin(), lat.sin(), lat.cos() * lon.cos()]
}

/// The copy of the image the analysis and the preview work on.
pub fn analysis_copy(rgb: &RgbImage) -> RgbImage {
    if rgb.dimensions() == ANALYSIS_SIZE {
        return rgb.clone();
    }
    image::imageops::resize(rgb, ANALYSIS_SIZE.0, ANALYSIS_SIZE.1, image::imageops::FilterType::Lanczos3)
}

/// The curve that only raises the very bright areas (linear luminance above 0.35).
fn highlight_gain(lum: f32, strength: f32) -> f32 {
    if strength <= 1.0 {
        return 1.0;
    }
    let s = smoothstep(0.35, 1.0, lum);
    1.0 + (strength - 1.0) * s * s
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
            out[y * w + x] = sum / span;
        }
    }
    out
}

/// Finds the sun: the densest blown-out patch of the sky (the upper half). Returns its position
/// from 0 to 1 and how sure the guess is (0 to 1), or `None` when nothing looks like the sun
/// (overcast, or the sun is hidden).
pub fn detect_sun(small: &RgbImage) -> Option<(f32, f32, f32)> {
    let (w, h) = (small.width() as usize, small.height() as usize);
    let table = srgb_to_linear_table();
    let rows = h / 2;
    let lum: Vec<f32> = small.as_raw()[..rows * w * 3]
        .chunks_exact(3)
        .map(|p| luminance([table[p[0] as usize], table[p[1] as usize], table[p[2] as usize]]))
        .collect();
    let peak = lum.iter().copied().fold(0f32, f32::max);
    if peak < 0.6 {
        return None;
    }
    let clipped: Vec<f32> = lum.iter().map(|v| if *v >= 0.92 * peak { 1.0 } else { 0.0 }).collect();
    // How many blown-out pixels there are around: a stray reflection weighs little, the sun a lot.
    let k = (w / 128).max(3);
    let density = box_blur(&clipped, w, rows, k);
    let (mut best, mut best_at) = (0f32, 0usize);
    for (i, v) in density.iter().enumerate() {
        if *v > best {
            best = *v;
            best_at = i;
        }
    }
    let confidence = (best * 1.5).min(1.0);
    if confidence < 0.25 {
        return None;
    }
    // The centre of the blown-out patch around the peak.
    let (px, py) = ((best_at % w) as isize, (best_at / w) as isize);
    let reach = (2 * k) as isize;
    let (mut sx, mut sy, mut count) = (0f32, 0f32, 0f32);
    for y in (py - reach).max(0)..=(py + reach).min(rows as isize - 1) {
        for dx in -reach..=reach {
            let x = (px + dx).rem_euclid(w as isize);
            if clipped[y as usize * w + x as usize] > 0.0 {
                sx += dx as f32;
                sy += (y - py) as f32;
                count += 1.0;
            }
        }
    }
    let (cx, cy) = if count > 0.0 { (px as f32 + sx / count, py as f32 + sy / count) } else { (px as f32, py as f32) };
    Some(((cx + 0.5).rem_euclid(w as f32) / w as f32, (cy + 0.5) / h as f32, confidence))
}

/// Everything needed to compute the HDR value of any pixel.
#[derive(Debug, Clone)]
pub struct Model {
    table: [f32; 256],
    highlights: f32,
    exposure: f32,
    sun_dir: [f32; 3],
    /// Outer edge of the sun's mask, as a cosine: nothing outside it is touched.
    cos_reach: f32,
    sun_size: f32,
    /// How much the sun's pixels are multiplied (1 = no sun).
    pub gain: f32,
    /// The sun found in the image: position and how sure.
    pub sun_auto: Option<(f32, f32, f32)>,
    /// The sun shown: the one placed by hand, or the one found.
    pub sun: Option<(f32, f32)>,
    /// The sun that is applied (none when its strength is 0).
    pub sun_used: Option<(f32, f32)>,
}

impl Model {
    /// Measures the sky on the analysis copy and prepares the conversion.
    pub fn new(small: &RgbImage, params: &Params) -> Model {
        let params = params.clamped();
        let sun_auto = detect_sun(small);
        let sun = params.sun_pos.or(sun_auto.map(|(u, v, _)| (u, v)));
        let sun_used = if params.sun_ratio > 0.0 { sun } else { None };
        let mut model = Model {
            table: srgb_to_linear_table(),
            highlights: params.highlights,
            exposure: 2f32.powf(params.exposure),
            sun_dir: [0.0, 1.0, 0.0],
            cos_reach: 2.0,
            sun_size: params.sun_size,
            gain: 1.0,
            sun_auto,
            sun,
            sun_used,
        };
        let Some((u, v)) = sun_used else { return model };
        model.sun_dir = direction(u, v);
        model.cos_reach = (params.sun_size * 1.6).to_radians().cos();
        model.gain = model.solve_gain(small, params.sun_ratio);
        model
    }

    /// How much of a pixel belongs to the sun: inside the disc and bright in the source.
    fn sun_weight(&self, linear_lum: f32, dir: [f32; 3]) -> f32 {
        let cos = dir[0] * self.sun_dir[0] + dir[1] * self.sun_dir[1] + dir[2] * self.sun_dir[2];
        if cos <= self.cos_reach {
            return 0.0;
        }
        let angle = cos.clamp(-1.0, 1.0).acos().to_degrees();
        smoothstep(self.sun_size * 1.6, self.sun_size * 0.6, angle) * smoothstep(0.25, 0.8, linear_lum)
    }

    /// The gain of the sun's pixels that makes the light the sun ADDS `ratio` times the light of
    /// everything else, measured on a horizontal surface (the ground of the scene). It is linear:
    /// added = (gain - 1) * sum(L * weight * mask).
    fn solve_gain(&self, small: &RgbImage, ratio: f32) -> f32 {
        if self.sun_dir[1] <= 0.01 {
            return 1.0; // below the horizon it lights nothing
        }
        let (w, h) = (small.width() as usize, small.height() as usize);
        let data = small.as_raw();
        let (sky, sun) = (0..h)
            .into_par_iter()
            .map(|y| {
                let v = (y as f32 + 0.5) / h as f32;
                let lat = (0.5 - v) * PI;
                let weight = lat.cos() * lat.sin().max(0.0);
                let (mut sky, mut sun) = (0f64, 0f64);
                if weight <= 0.0 {
                    return (sky, sun);
                }
                for x in 0..w {
                    let i = (y * w + x) * 3;
                    let linear = [self.table[data[i] as usize], self.table[data[i + 1] as usize], self.table[data[i + 2] as usize]];
                    let lin_lum = luminance(linear);
                    let lum = lin_lum * highlight_gain(lin_lum, self.highlights) * self.exposure;
                    let mask = self.sun_weight(lin_lum, direction((x as f32 + 0.5) / w as f32, v));
                    sky += (lum * weight) as f64;
                    sun += (lum * weight * mask) as f64;
                }
                (sky, sun)
            })
            .reduce(|| (0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));
        if sun <= 1e-9 {
            return 1.0;
        }
        (1.0 + ratio as f64 * sky / sun) as f32
    }

    /// The HDR value of a pixel of the source that lies in direction `dir`.
    pub fn shade(&self, pixel: [u8; 3], dir: [f32; 3]) -> [f32; 3] {
        let linear = [self.table[pixel[0] as usize], self.table[pixel[1] as usize], self.table[pixel[2] as usize]];
        let lum = luminance(linear);
        let mut gain = highlight_gain(lum, self.highlights) * self.exposure;
        if self.gain > 1.0 {
            gain *= 1.0 + (self.gain - 1.0) * self.sun_weight(lum, dir);
        }
        [linear[0] * gain, linear[1] * gain, linear[2] * gain]
    }

    /// The HDRI as it would look photographed with `view_ev` stops of exposure compensation, and
    /// what to tell about it.
    pub fn preview(&self, small: &RgbImage, view_ev: f32) -> (RgbImage, PreviewInfo) {
        let (w, h) = small.dimensions();
        let look = 2f32.powf(view_ev);
        let values: Vec<[f32; 3]> = (0..h as usize)
            .into_par_iter()
            .flat_map_iter(|y| {
                let v = (y as f32 + 0.5) / h as f32;
                (0..w as usize).map(move |x| {
                    let p = small.get_pixel(x as u32, y as u32).0;
                    self.shade(p, direction((x as f32 + 0.5) / w as f32, v))
                })
            })
            .collect();
        let mut image = vec![0u8; values.len() * 3];
        image.par_chunks_mut(3).zip(values.par_iter()).for_each(|(out, c)| {
            for k in 0..3 {
                out[k] = linear_to_srgb(c[k] * look);
            }
        });
        let peak = values.iter().map(|c| c[0].max(c[1]).max(c[2])).fold(0f32, f32::max);
        let mut lums: Vec<f32> = values.iter().map(|c| luminance(*c)).filter(|l| *l > 0.0).collect();
        let median = if lums.is_empty() {
            1e-6
        } else {
            let middle = lums.len() / 2;
            *lums.select_nth_unstable_by(middle, |a, b| a.total_cmp(b)).1
        };
        let info = PreviewInfo {
            sun: self.sun.map(|(u, v)| SunPlace::new(u, v)),
            sun_auto: self.sun_auto.is_some(),
            sun_confidence: self.sun_auto.map_or(0.0, |(_, _, c)| round_to(c, 2)),
            sun_gain_ev: if self.gain > 1.0 { round_to(self.gain.log2(), 1) } else { 0.0 },
            peak: round_to(peak, 1),
            range_ev: round_to((peak.max(1e-6) / median.max(1e-6)).log2(), 1),
            below_horizon: self.sun.is_some_and(|(_, v)| v >= 0.5),
        };
        (RgbImage::from_raw(w, h, image).expect("buffer has the right size"), info)
    }
}

fn round_to(value: f32, decimals: i32) -> f64 {
    let scale = 10f64.powi(decimals);
    (value as f64 * scale).round() / scale
}

/// Where the sun is, for the interface.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct SunPlace {
    pub u: f64,
    pub v: f64,
    /// Degrees from the centre of the image, turning right.
    pub azimuth: f64,
    /// Degrees above the horizon.
    pub elevation: f64,
}

impl SunPlace {
    fn new(u: f32, v: f32) -> SunPlace {
        SunPlace {
            u: round_to(u, 4),
            v: round_to(v, 4),
            azimuth: round_to(((u - 0.5) * 360.0).rem_euclid(360.0), 1),
            elevation: round_to((0.5 - v) * 180.0, 1),
        }
    }
}

/// What the preview tells about the HDRI.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PreviewInfo {
    pub sun: Option<SunPlace>,
    pub sun_auto: bool,
    pub sun_confidence: f64,
    /// How many stops the sun was raised.
    pub sun_gain_ev: f64,
    pub peak: f64,
    /// Stops between the brightest value and the median one.
    pub range_ev: f64,
    pub below_horizon: bool,
}

/// The size an HDRI of `width` (None = the original) will have from a source of `source_width`:
/// never larger than the source (no resolution is invented), always even and 2:1.
pub fn output_size(source_width: u32, width: Option<u32>) -> (u32, u32) {
    let w = width.unwrap_or(source_width).min(source_width).max(2);
    let w = w - w % 2;
    (w, w / 2)
}

/// `panorama.jpg` becomes `panorama_hdri.exr` next to it.
pub fn output_path(source: &Path, format: HdrFormat) -> PathBuf {
    let stem = source.file_stem().and_then(|s| s.to_str()).unwrap_or("panorama");
    source.with_file_name(format!("{stem}_hdri.{}", format.extension()))
}

/// Converts and writes the HDRI. `progress` gets 0 to 1. Returns the size written.
pub fn write(
    source: &Source,
    params: &Params,
    width: Option<u32>,
    format: HdrFormat,
    path: &Path,
    cancel: &AtomicBool,
    progress: &(dyn Fn(f32) + Sync),
) -> Result<(u32, u32), Error> {
    let small = analysis_copy(&source.rgb);
    let model = Model::new(&small, params);
    drop(small);
    let (w, h) = output_size(source.rgb.width(), width);
    let filter = image::imageops::FilterType::Lanczos3;
    let resized;
    let rgb = if source.rgb.dimensions() == (w, h) {
        &source.rgb
    } else {
        resized = image::imageops::resize(&source.rgb, w, h, filter);
        &resized
    };
    let alpha = source
        .alpha
        .as_ref()
        .map(|a| if a.dimensions() == (w, h) { a.clone() } else { image::imageops::resize(a, w, h, filter) });
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut partial = path.as_os_str().to_owned();
    partial.push(".part");
    let partial = PathBuf::from(partial);
    let result = match format {
        HdrFormat::Exr => write_exr(&model, rgb, alpha.as_ref(), &partial, cancel, progress),
        HdrFormat::Hdr => write_radiance(&model, rgb, &partial, cancel, progress),
    };
    let result = result.and_then(|()| if cancel.load(Ordering::Relaxed) { Err(Error::Cancelled) } else { Ok(()) });
    match result {
        Ok(()) => {
            let _ = fs::remove_file(path);
            fs::rename(&partial, path)?;
            progress(1.0);
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

fn write_exr(
    model: &Model,
    rgb: &RgbImage,
    alpha: Option<&GrayImage>,
    path: &Path,
    cancel: &AtomicBool,
    progress: &(dyn Fn(f32) + Sync),
) -> Result<(), crate::hdri::Error> {
    use exr::prelude::*;
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    let directions = Directions::new(rgb.width(), rgb.height());
    let data = rgb.as_raw();
    // Once cancelled the rest is written as black, quickly, and the file is thrown away.
    let value = |x: usize, y: usize| {
        if cancel.load(Ordering::Relaxed) {
            return [f16::ZERO; 3];
        }
        let i = (y * w + x) * 3;
        let c = model.shade([data[i], data[i + 1], data[i + 2]], directions.at(x, y));
        [f16::from_f32(c[0].min(MAX_VALUE)), f16::from_f32(c[1].min(MAX_VALUE)), f16::from_f32(c[2].min(MAX_VALUE))]
    };
    let attributes = LayerAttributes::default();
    let encoding = Encoding::SMALL_LOSSLESS;
    let report = |done: f64| progress(0.98 * done as f32);
    let result = match alpha {
        Some(alpha) => {
            let alpha = alpha.as_raw();
            let channels = SpecificChannels::rgba(|Vec2(x, y)| {
                let c = value(x, y);
                (c[0], c[1], c[2], f16::from_f32(alpha[y * w + x] as f32 / 255.0))
            });
            Image::from_layer(Layer::new((w, h), attributes, encoding, channels)).write().on_progress(report).to_file(path)
        }
        None => {
            let channels = SpecificChannels::rgb(|Vec2(x, y)| {
                let c = value(x, y);
                (c[0], c[1], c[2])
            });
            Image::from_layer(Layer::new((w, h), attributes, encoding, channels)).write().on_progress(report).to_file(path)
        }
    };
    result.map_err(|e| match e {
        exr::error::Error::Io(io) => crate::hdri::Error::Io(io),
        other => crate::hdri::Error::Image(other.to_string()),
    })
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

fn write_radiance(
    model: &Model,
    rgb: &RgbImage,
    path: &Path,
    cancel: &AtomicBool,
    progress: &(dyn Fn(f32) + Sync),
) -> Result<(), Error> {
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    let directions = Directions::new(rgb.width(), rgb.height());
    let data = rgb.as_raw();
    let mut file = BufWriter::with_capacity(1 << 20, fs::File::create(path)?);
    write!(file, "#?RADIANCE\nSOFTWARE=OpenView {}\nFORMAT=32-bit_rle_rgbe\n\n-Y {h} +X {w}\n", crate::VERSION)?;
    // Scanlines are independent: encode a batch at a time in parallel, write them in order.
    let rows: Vec<usize> = (0..h).collect();
    for batch in rows.chunks(256) {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
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
        progress(0.98 * (batch[batch.len() - 1] + 1) as f32 / h as f32);
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
            if cos > 2.5f32.to_radians().cos() {
                Rgb([255, 255, 255])
            } else if y < h / 2 {
                Rgb([90, 140, 210])
            } else {
                Rgb([90, 90, 90])
            }
        })
    }

    fn never() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn colours_survive_linear_and_back() {
        let table = srgb_to_linear_table();
        for v in [0u8, 1, 50, 128, 200, 255] {
            assert_eq!(linear_to_srgb(table[v as usize]), v);
        }
    }

    #[test]
    fn the_sun_is_found_where_it_is() {
        let image = sunny(1024, 0.7, 0.25);
        let (x, y, confidence) = detect_sun(&image).expect("sun found");
        assert!((x - 0.7).abs() < 0.01 && (y - 0.25).abs() < 0.01, "found at {x},{y}");
        assert!(confidence > 0.5);
    }

    #[test]
    fn the_sun_is_found_across_the_seam() {
        let image = sunny(1024, 0.001, 0.3);
        let (x, y, _) = detect_sun(&image).expect("sun found");
        let distance = (x - 0.001).abs().min(1.0 - (x - 0.001).abs());
        assert!(distance < 0.01 && (y - 0.3).abs() < 0.01, "found at {x},{y}");
    }

    #[test]
    fn an_overcast_sky_has_no_sun() {
        let grey = RgbImage::from_pixel(512, 256, Rgb([170, 175, 180]));
        assert_eq!(detect_sun(&grey), None);
    }

    #[test]
    fn a_reflection_below_the_horizon_is_not_the_sun() {
        let mut image = RgbImage::from_pixel(1024, 512, Rgb([100, 140, 200]));
        for y in 380..400 {
            for x in 500..520 {
                image.put_pixel(x, y, Rgb([255, 255, 255]));
            }
        }
        assert_eq!(detect_sun(&image), None);
    }

    #[test]
    fn the_sun_gives_the_light_that_was_asked_for() {
        let image = sunny(1024, 0.5, 0.2);
        let params = Params { sun_ratio: 5.0, ..Params::default() };
        let model = Model::new(&image, &params);
        assert!(model.gain > 1.0);
        let none = Model::new(&image, &Params { sun_ratio: 0.0, ..params });
        // The light on a horizontal surface, with and without the sun.
        let light = |m: &Model| {
            let (w, h) = (1024usize, 512usize);
            let mut total = 0f64;
            for y in 0..h / 2 {
                let v = (y as f32 + 0.5) / h as f32;
                let lat = (0.5 - v) * PI;
                for x in 0..w {
                    let c = m.shade(image.get_pixel(x as u32, y as u32).0, direction((x as f32 + 0.5) / w as f32, v));
                    total += (luminance(c) * lat.sin() * lat.cos()) as f64;
                }
            }
            total
        };
        let ratio = (light(&model) - light(&none)) / light(&none);
        assert!((ratio - 5.0).abs() < 0.1, "the sun adds {ratio} times the rest");
    }

    #[test]
    fn the_preview_tells_about_the_sun() {
        let image = sunny(1024, 0.75, 0.25);
        let model = Model::new(&image, &Params::default());
        let (normal, info) = model.preview(&image, 0.0);
        assert_eq!(normal.dimensions(), (1024, 512));
        let sun = info.sun.expect("a sun");
        assert!((sun.azimuth - 90.0).abs() < 2.0 && (sun.elevation - 45.0).abs() < 2.0, "{sun:?}");
        assert!(info.sun_auto && info.sun_gain_ev > 3.0 && info.range_ev > 5.0 && !info.below_horizon, "{info:?}");
        // At -8 stops only the sun is left.
        let (dark, _) = model.preview(&image, -8.0);
        let at = |x: f32, y: f32| dark.get_pixel((x * 1024.0) as u32, (y * 512.0) as u32).0;
        assert!(at(0.75, 0.25)[0] > 150, "{:?}", at(0.75, 0.25));
        assert!(at(0.1, 0.1)[2] < 20 && at(0.3, 0.8)[0] < 10);
    }

    #[test]
    fn a_sun_placed_by_hand_is_used() {
        let image = sunny(1024, 0.5, 0.2);
        let model = Model::new(&image, &Params { sun_pos: Some((0.5, 0.2)), ..Params::default() });
        assert_eq!(model.sun, Some((0.5, 0.2)));
        let (_, info) = model.preview(&image, 0.0);
        assert!(info.sun_auto, "the automatic sun is still reported");
        let below = Model::new(&image, &Params { sun_pos: Some((0.5, 0.7)), ..Params::default() });
        assert_eq!(below.gain, 1.0);
        assert!(below.preview(&image, 0.0).1.below_horizon);
    }

    #[test]
    fn without_sun_only_the_highlights_change() {
        let image = sunny(512, 0.5, 0.2);
        let model = Model::new(&image, &Params { sun_ratio: 0.0, ..Params::default() });
        assert_eq!((model.gain, model.sun_used), (1.0, None));
        let white = model.shade([255, 255, 255], direction(0.5, 0.2));
        assert!((white[0] - 3.0).abs() < 1e-4, "{white:?}");
        let mid = model.shade([100, 100, 100], direction(0.1, 0.6));
        assert!((mid[0] - srgb_to_linear_table()[100]).abs() < 1e-6);
    }

    #[test]
    fn parameters_are_kept_in_range() {
        let wild = Params { sun_ratio: 99.0, sun_size: 0.0, highlights: f32::NAN, exposure: -9.0, sun_pos: Some((2.0, -1.0)) };
        let tame = wild.clamped();
        assert_eq!((tame.sun_ratio, tame.sun_size, tame.highlights, tame.exposure), (20.0, 0.3, 3.0, -5.0));
        assert_eq!(tame.sun_pos, Some((1.0, 0.0)));
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
    fn sizes_never_grow_and_stay_two_to_one() {
        assert_eq!(output_size(16384, Some(8192)), (8192, 4096));
        assert_eq!(output_size(3000, Some(8192)), (3000, 1500));
        assert_eq!(output_size(3001, None), (3000, 1500));
    }

    #[test]
    fn files_are_written_in_both_formats_with_progress() {
        let dir = std::env::temp_dir().join(format!("openview-hdri-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let source = Source { rgb: sunny(256, 0.5, 0.2), alpha: None };
        let exr_path = dir.join("p_hdri.exr");
        let last = std::sync::Mutex::new(0f32);
        let report = |f: f32| *last.lock().unwrap() = f;
        let size = write(&source, &Params::default(), Some(128), HdrFormat::Exr, &exr_path, &never(), &report).unwrap();
        assert_eq!(size, (128, 64));
        assert_eq!(*last.lock().unwrap(), 1.0);
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
        assert!(peak > 20.0, "the sun is missing from the file: {peak}");

        let hdr_path = dir.join("p_hdri.hdr");
        assert_eq!(write(&source, &Params::default(), None, HdrFormat::Hdr, &hdr_path, &never(), &|_| {}).unwrap(), (256, 128));
        let bytes = fs::read(&hdr_path).unwrap();
        assert!(bytes.starts_with(b"#?RADIANCE\n"));
        assert!(bytes.windows(15).any(|w| w == b"-Y 128 +X 256\n\x02"));
        assert_eq!(output_path(Path::new("a/b/pano.jpg"), HdrFormat::Exr), Path::new("a/b/pano_hdri.exr"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn transparency_is_kept_in_the_exr() {
        let dir = std::env::temp_dir().join(format!("openview-alpha-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let rgba = image::RgbaImage::from_fn(128, 64, |_, y| image::Rgba([90, 140, 210, if y < 32 { 0 } else { 255 }]));
        let png = dir.join("cut.png");
        rgba.save(&png).unwrap();
        let source = load(&png).expect("loaded");
        assert!(source.alpha.is_some());
        let out = dir.join("cut_hdri.exr");
        write(&source, &Params::default(), None, HdrFormat::Exr, &out, &never(), &|_| {}).unwrap();
        let back = exr::prelude::read_first_rgba_layer_from_file(
            &out,
            |resolution, _| vec![vec![0f32; resolution.width()]; resolution.height()],
            |rows: &mut Vec<Vec<f32>>, position, (_, _, _, a): (f32, f32, f32, f32)| rows[position.y()][position.x()] = a,
        )
        .unwrap();
        let alpha = &back.layer_data.channel_data.pixels;
        assert_eq!((alpha[0][0], alpha[63][0]), (0.0, 1.0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn images_that_are_not_panoramas_are_refused() {
        let dir = std::env::temp_dir().join(format!("openview-refuse-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let square = dir.join("square.png");
        RgbImage::new(200, 200).save(&square).unwrap();
        assert!(matches!(load(&square), Err(Error::NotSpherical(200, 200))));
        assert!(matches!(dimensions(&square), Err(Error::NotSpherical(200, 200))));
        let tiny = dir.join("tiny.png");
        RgbImage::new(40, 20).save(&tiny).unwrap();
        assert!(matches!(load(&tiny), Err(Error::TooSmall)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cancelled_conversion_leaves_nothing() {
        let dir = std::env::temp_dir().join(format!("openview-cancel-{}", std::process::id()));
        let source = Source { rgb: sunny(256, 0.5, 0.2), alpha: None };
        let stop = AtomicBool::new(true);
        for format in [HdrFormat::Exr, HdrFormat::Hdr] {
            let out = dir.join(format!("c_hdri.{}", format.extension()));
            assert!(matches!(write(&source, &Params::default(), None, format, &out, &stop, &|_| {}), Err(Error::Cancelled)));
            assert!(!out.exists());
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
