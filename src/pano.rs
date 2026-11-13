// SPDX-License-Identifier: GPL-3.0-or-later
//! Downloads a panorama or a photo: fetches the image (joining tiles when the service hands it out
//! in pieces), applies the touch-ups the user asked for and writes the file.
//!
//! Fetching user images in one piece, keeping the original JPEG when nothing was changed, and the
//! way flat photos are downloaded follow the original OpenView by Daniel Castellanos (MIT, see
//! NOTICE).

use std::fs;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ImageEncoder, RgbImage};
use serde_json::json;

use crate::extras;
use crate::geom::{self, Mat3, Quality, View};
use crate::links::Kind;
use crate::service::{self, Client, PanoInfo};

const WORKERS: usize = 16;
const TRIES: u32 = 4;
pub const JPEG_QUALITY: u8 = 97;
/// The longest side a crop may have.
pub const MAX_CROP_SIDE: u32 = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Jpg,
    Png,
    Tif,
}

impl Format {
    pub const ALL: [Format; 3] = [Format::Jpg, Format::Png, Format::Tif];

    pub fn extension(self) -> &'static str {
        match self {
            Format::Jpg => "jpg",
            Format::Png => "png",
            Format::Tif => "tif",
        }
    }

    pub fn parse(text: &str) -> Option<Format> {
        match text.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Some(Format::Jpg),
            "png" => Some(Format::Png),
            "tif" | "tiff" => Some(Format::Tif),
            _ => None,
        }
    }
}

/// What the user chose for a download.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub out_dir: PathBuf,
    pub format: Format,
    /// Turn the image so its centre looks north.
    pub north: bool,
    /// Straighten panoramas taken with a leaning camera.
    pub level: bool,
    /// Cover the ground under the camera.
    pub cover_ground: bool,
    /// Complete 360 photos that do not cover the whole sphere.
    pub complete: bool,
    /// Also write a `.json` with position, date, heading, author and link.
    pub json: bool,
    /// Download a smaller version: the largest one not wider than this.
    pub max_width: Option<u32>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            out_dir: PathBuf::from("."),
            format: Format::Jpg,
            north: false,
            level: true,
            cover_ground: false,
            complete: true,
            json: false,
            max_width: None,
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Cancelled,
    Service(service::Error),
    Image(String),
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", crate::text::Lang::En.pano(self))
    }
}

impl std::error::Error for Error {}

impl From<service::Error> for Error {
    fn from(e: service::Error) -> Self {
        Error::Service(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<image::ImageError> for Error {
    fn from(e: image::ImageError) -> Self {
        match e {
            image::ImageError::IoError(io) => Error::Io(io),
            other => Error::Image(other.to_string()),
        }
    }
}

/// What a job is doing, for the progress bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    /// Pieces fetched so far, of how many.
    Tiles(usize, usize),
    Completing,
    Levelling,
    CoveringGround,
    Saving,
}

impl Step {
    pub fn label(self) -> String {
        match self {
            Step::Tiles(done, total) => format!("Downloading {done}/{total}"),
            Step::Completing => "Completing the sphere".into(),
            Step::Levelling => "Levelling the horizon".into(),
            Step::CoveringGround => "Covering the ground".into(),
            Step::Saving => "Saving".into(),
        }
    }

    /// How much of the download itself is done, 0 to 1; everything after the download is 1.
    pub fn fraction(self) -> f32 {
        match self {
            Step::Tiles(done, total) => done as f32 / total.max(1) as f32,
            _ => 1.0,
        }
    }
}

/// The handles a running job needs: a way to be cancelled and a way to report progress.
pub struct Control<'a> {
    pub cancel: &'a AtomicBool,
    pub progress: &'a (dyn Fn(Step) + Sync),
}

impl Control<'_> {
    pub fn check(&self) -> Result<(), Error> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

fn decode(bytes: &[u8]) -> Result<RgbImage, Error> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    reader.no_limits();
    Ok(reader.decode()?.into_rgb8())
}

fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xD8, 0xFF])
}

/// Fetches a file, trying again when the network (not the service's answer) fails.
fn fetch(client: &Client, url: &str, tries: u32, cancel: &AtomicBool) -> Result<Vec<u8>, Error> {
    let mut last = None;
    for attempt in 0..tries {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(1000 * attempt as u64));
        }
        match client.get_bytes(url) {
            Ok(bytes) => return Ok(bytes),
            // The service says it has no such file: asking again will not change that.
            Err(e @ service::Error::NotFound) => return Err(e.into()),
            Err(e) => last = Some(e),
        }
    }
    Err(last.map_or(Error::Image("no answer".into()), Error::Service))
}

fn blit(canvas: &mut RgbImage, tile: &RgbImage, left: u32, top: u32) {
    let (cw, ch) = canvas.dimensions();
    if left >= cw || top >= ch {
        return;
    }
    let copy_w = tile.width().min(cw - left) as usize;
    let copy_h = tile.height().min(ch - top) as usize;
    let (tw, cw) = (tile.width() as usize, cw as usize);
    let src = tile.as_raw();
    let dst: &mut [u8] = canvas.as_mut();
    for row in 0..copy_h {
        let s = row * tw * 3;
        let d = ((top as usize + row) * cw + left as usize) * 3;
        dst[d..d + copy_w * 3].copy_from_slice(&src[s..s + copy_w * 3]);
    }
}

/// Fetches and joins the tiles of one zoom level. The result is the image exactly as the camera
/// took it. The second value is the number of tiles the service did not have.
pub fn fetch_tiles(client: &Client, info: &PanoInfo, zoom: usize, control: &Control) -> Result<(RgbImage, usize), Error> {
    let (w, h) = *info.sizes.get(zoom).ok_or_else(|| Error::Image("no such zoom level".into()))?;
    let (tw, th) = (info.tile.0.max(1), info.tile.1.max(1));
    let (cols, rows) = (w.div_ceil(tw), h.div_ceil(th));
    let total = (cols * rows) as usize;
    let canvas = Mutex::new(RgbImage::new(w, h));
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let missing = AtomicUsize::new(0);
    let failure: Mutex<Option<Error>> = Mutex::new(None);
    let stop = AtomicBool::new(false);
    (control.progress)(Step::Tiles(0, total));

    std::thread::scope(|scope| {
        for _ in 0..WORKERS.min(total) {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= total || stop.load(Ordering::Relaxed) || control.cancel.load(Ordering::Relaxed) {
                    return;
                }
                let (x, y) = (index as u32 % cols, index as u32 / cols);
                let url = service::tile_url(&info.id, info.kind, zoom, x, y);
                match fetch(client, &url, TRIES, control.cancel).and_then(|bytes| decode(&bytes)) {
                    Ok(tile) => {
                        let (need_w, need_h) = (tw.min(w - x * tw), th.min(h - y * th));
                        let tile = if tile.width() >= need_w && tile.height() >= need_h {
                            tile
                        } else {
                            // Some tiles arrive at a lower resolution than announced: never leave gaps.
                            image::imageops::resize(&tile, tw, th, image::imageops::FilterType::Lanczos3)
                        };
                        blit(&mut canvas.lock().unwrap(), &tile, x * tw, y * th);
                    }
                    // The service has no tile here: the picture does not reach this far.
                    Err(Error::Service(service::Error::NotFound)) => {
                        missing.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        stop.store(true, Ordering::Relaxed);
                        failure.lock().unwrap().get_or_insert(e);
                        return;
                    }
                }
                let finished = done.fetch_add(1, Ordering::Relaxed) + 1;
                (control.progress)(Step::Tiles(finished, total));
            });
        }
    });

    control.check()?;
    if let Some(e) = failure.into_inner().unwrap() {
        return Err(e);
    }
    let missing = missing.into_inner();
    if missing * 2 > total {
        return Err(Error::Service(service::Error::NotFound));
    }
    Ok((canvas.into_inner().unwrap(), missing))
}

/// Fetches an image the service hands out in one piece (a user's 360 photo or a flat photo) at the
/// given size. Returns the picture and the bytes exactly as they arrived.
pub fn fetch_whole(client: &Client, info: &PanoInfo, width: u32, height: u32, cancel: &AtomicBool) -> Result<(RgbImage, Vec<u8>), Error> {
    let mut last = Error::Service(service::Error::NotFound);
    for url in info.whole_image_urls(width, height) {
        match fetch(client, &url, 2, cancel) {
            Ok(bytes) => match decode(&bytes) {
                Ok(image) => return Ok((image, bytes)),
                Err(e) => last = e,
            },
            Err(e @ Error::Service(service::Error::NotFound)) => last = e,
            // No network: the other addresses would fail the same way.
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

/// A whole 360 image, untouched.
pub struct Fetched {
    pub image: RgbImage,
    /// The file as the service sent it, when it came in one piece.
    pub raw: Option<Vec<u8>>,
    /// Tiles the service did not have (they are black in the image).
    pub missing_tiles: usize,
}

/// The 360 image of a zoom level exactly as the camera took it: in one piece when the service
/// offers that (user photos), tile by tile otherwise.
pub fn fetch_pano(client: &Client, info: &PanoInfo, zoom: usize, control: &Control) -> Result<Fetched, Error> {
    if info.kind == Kind::User {
        let (w, h) = *info.sizes.get(zoom).ok_or_else(|| Error::Image("no such zoom level".into()))?;
        (control.progress)(Step::Tiles(0, 1));
        match fetch_whole(client, info, w, h, control.cancel) {
            Ok((image, raw)) => {
                (control.progress)(Step::Tiles(1, 1));
                return Ok(Fetched { image, raw: Some(raw), missing_tiles: 0 });
            }
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e @ Error::Service(service::Error::Network(_))) => return Err(e),
            // Not available in one piece: fall back to its tiles.
            Err(_) => {}
        }
    }
    let (image, missing_tiles) = fetch_tiles(client, info, zoom, control)?;
    Ok(Fetched { image, raw: None, missing_tiles })
}

fn jpeg_bytes(image: &RgbImage, quality: u8) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality).write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(out)
}

/// A small version of the image as a JPEG, for the card and the viewer.
pub fn thumbnail(client: &Client, info: &PanoInfo, width: u32, cancel: &AtomicBool) -> Result<Vec<u8>, Error> {
    let (full_w, full_h) = info.full_size();
    if info.flat {
        if full_w == 0 {
            // A photo pasted as a bare image address: its size is not known.
            let base = info.image_url.as_deref().ok_or(Error::Service(service::Error::NotFound))?;
            return fetch(client, &format!("{base}=w{width}"), 2, cancel);
        }
        let scale = (width as f64 / full_w.max(full_h) as f64).min(1.0);
        let (w, h) = (((full_w as f64 * scale).round() as u32).max(1), ((full_h as f64 * scale).round() as u32).max(1));
        return fetch_whole(client, info, w, h, cancel).map(|(_, raw)| raw);
    }
    if info.kind == Kind::User {
        let w = width.min(full_w);
        let h = ((w as f64 * full_h as f64 / full_w as f64).round() as u32).max(1);
        if let Ok((_, raw)) = fetch_whole(client, info, w, h, cancel) {
            return Ok(raw);
        }
    }
    let control = Control { cancel, progress: &|_| {} };
    let (image, _) = fetch_tiles(client, info, info.zoom_for(Some(width)), &control)?;
    jpeg_bytes(&image, 85)
}

/// The same as [`thumbnail`], decoded.
pub fn fetch_preview(client: &Client, info: &PanoInfo, max_width: u32, cancel: &AtomicBool) -> Result<RgbImage, Error> {
    decode(&thumbnail(client, info, max_width, cancel)?)
}

/// A tiny picture of a capture for the strip of dates: its smallest tile, without the padding.
pub fn mini(client: &Client, id: &str, kind: Kind) -> Result<Vec<u8>, Error> {
    let never = AtomicBool::new(false);
    let bytes = fetch(client, &service::tile_url(id, kind, 0, 0, 0), 2, &never)?;
    let tile = extras::trim_padding(&decode(&bytes)?);
    let small = image::imageops::resize(&tile, 256, 128, image::imageops::FilterType::Lanczos3);
    jpeg_bytes(&small, 82)
}

fn short_id(id: &str) -> String {
    let clean: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    // The first characters of user uploads are the same for all of them: the end tells them apart.
    if clean.len() > 12 {
        clean[clean.len() - 12..].to_string()
    } else {
        clean
    }
}

/// The file name (without suffixes or extension) of a panorama or a photo.
pub fn file_stem(info: &PanoInfo) -> String {
    let mut stem = String::from(match (info.flat, info.kind) {
        (true, _) => "photo",
        (false, Kind::Official) => "street",
        (false, Kind::User) => "photo360",
    });
    let date = info.date_text();
    if !date.is_empty() {
        stem.push('_');
        stem.push_str(&date);
    }
    if let (Some(lat), Some(lon)) = (info.lat, info.lon) {
        stem.push_str(&format!("_{lat:.5}_{lon:.5}"));
    }
    stem.push('_');
    stem.push_str(&short_id(&info.id));
    stem
}

/// The suffixes that tell versions of the same panorama apart, so they do not replace each other.
pub fn suffixes(info: &PanoInfo, options: &Options, zoom: usize) -> String {
    let mut text = String::new();
    if zoom + 1 < info.sizes.len() {
        text.push_str(&format!("_{}px", info.sizes[zoom].0));
    }
    if options.cover_ground {
        text.push_str("_nadir");
    }
    if options.north && info.heading.is_some() {
        text.push_str("_north");
    }
    text
}

/// Writes a file under a temporary name and gives it its final name only when it is complete.
fn write_whole(path: &Path, write: impl FnOnce(&Path) -> Result<(), Error>) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut partial = path.as_os_str().to_owned();
    partial.push(".part");
    let partial = PathBuf::from(partial);
    match write(&partial) {
        Ok(()) => {
            let _ = fs::remove_file(path);
            fs::rename(&partial, path)?;
            Ok(())
        }
        Err(e) => {
            let _ = fs::remove_file(&partial);
            Err(e)
        }
    }
}

/// Writes an image in the chosen format: JPG at quality 97 without colour subsampling, PNG, or
/// TIFF with LZW compression (both without loss).
pub fn save(image: &RgbImage, path: &Path, format: Format) -> Result<(), Error> {
    write_whole(path, |partial| {
        let writer = BufWriter::with_capacity(1 << 20, fs::File::create(partial)?);
        let (w, h) = image.dimensions();
        let colour = image::ExtendedColorType::Rgb8;
        match format {
            Format::Jpg => JpegEncoder::new_with_quality(writer, JPEG_QUALITY).write_image(image.as_raw(), w, h, colour)?,
            Format::Png => PngEncoder::new_with_quality(writer, CompressionType::Default, FilterType::Adaptive)
                .write_image(image.as_raw(), w, h, colour)?,
            Format::Tif => {
                use tiff::encoder::{colortype::RGB8, Compression, TiffEncoder};
                TiffEncoder::new(writer)
                    .and_then(|encoder| {
                        encoder.with_compression(Compression::Lzw).write_image::<RGB8>(w, h, image.as_raw())
                    })
                    .map_err(|e| match e {
                        tiff::TiffError::IoError(io) => Error::Io(io),
                        other => Error::Image(other.to_string()),
                    })?
            }
        }
        Ok(())
    })
}

/// Writes bytes exactly as they are.
fn save_raw(bytes: &[u8], path: &Path) -> Result<(), Error> {
    write_whole(path, |partial| Ok(fs::write(partial, bytes)?))
}

/// What was done to an image before saving it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Touches {
    pub levelled: bool,
    pub completed: bool,
    pub ground_covered: bool,
    pub north: bool,
}

/// The data file that can go with a panorama or a photo.
pub fn sidecar(info: &PanoInfo, link: &str, size: (u32, u32), touches: Touches) -> serde_json::Value {
    json!({
        "id": info.id,
        "kind": match (info.flat, info.kind) {
            (true, _) => "photo",
            (false, Kind::Official) => "street",
            (false, Kind::User) => "user",
        },
        "date": info.date_text(),
        "latitude": info.lat,
        "longitude": info.lon,
        "address": info.address,
        "heading": info.heading,
        "heading_of_centre": if touches.north { Some(0.0) } else { info.heading },
        "tilt": info.tilt,
        "roll": info.roll,
        "levelled": touches.levelled,
        "completed_to_2_1": touches.completed,
        "ground_covered": touches.ground_covered,
        "aligned_north": touches.north,
        "width": size.0,
        "height": size.1,
        "author": info.author,
        "copyright": info.copyright,
        "link": if link.is_empty() { info.map_link() } else { link.to_string() },
        "app": format!("OpenView {}", crate::VERSION),
    })
}

/// The matrix that straightens this panorama, when there is something to straighten.
pub fn level_of(info: &PanoInfo) -> Option<Mat3> {
    if info.flat {
        return None;
    }
    geom::level_matrix(info.tilt?, info.roll?)
}

/// What a finished download left behind.
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    pub path: PathBuf,
    pub size: (u32, u32),
    /// Tiles the service did not have (they are black in the image).
    pub missing_tiles: usize,
}

/// Downloads a panorama or a photo with the chosen options and writes it. `link` is what the user
/// pasted, for the data file.
pub fn download(client: &Client, info: &PanoInfo, link: &str, options: &Options, control: &Control) -> Result<Saved, Error> {
    // Find out that the folder cannot be written before downloading a hundred megabytes.
    fs::create_dir_all(&options.out_dir)?;
    if info.flat {
        return download_photo(client, info, link, options, control);
    }
    let zoom = info.zoom_for(options.max_width);
    let Fetched { mut image, mut raw, missing_tiles } = fetch_pano(client, info, zoom, control)?;
    control.check()?;

    let mut touches = Touches::default();
    if options.complete && extras::is_partial(image.width(), image.height()) {
        (control.progress)(Step::Completing);
        if let Some(full) = extras::complete_sphere(&image) {
            image = full;
            touches.completed = true;
        }
        control.check()?;
    }
    // Levelling and the ground patch only make sense on a whole sphere.
    let spherical = !extras::is_partial(image.width(), image.height());
    if options.level && spherical {
        if let Some(matrix) = level_of(info) {
            (control.progress)(Step::Levelling);
            image = geom::reproject(&image, &matrix);
            touches.levelled = true;
            control.check()?;
        }
    }
    if options.cover_ground && spherical {
        (control.progress)(Step::CoveringGround);
        extras::patch_nadir(&mut image, extras::NADIR_ANGLE_DEG);
        touches.ground_covered = true;
    }
    if options.north {
        if let Some(heading) = info.heading {
            image = extras::face_north(&image, heading);
            touches.north = true;
        }
    }
    if touches != Touches::default() {
        raw = None;
    }
    control.check()?;

    (control.progress)(Step::Saving);
    let name = format!("{}{}", file_stem(info), suffixes(info, options, zoom));
    let path = options.out_dir.join(format!("{name}.{}", options.format.extension()));
    match raw {
        // The original JPEG, not recompressed.
        Some(bytes) if options.format == Format::Jpg && is_jpeg(&bytes) => save_raw(&bytes, &path)?,
        _ => save(&image, &path, options.format)?,
    }
    if options.json {
        let data = sidecar(info, link, image.dimensions(), touches);
        fs::write(options.out_dir.join(format!("{name}.json")), serde_json::to_string_pretty(&data).unwrap_or_default())?;
    }
    Ok(Saved { path, size: image.dimensions(), missing_tiles })
}

/// Downloads a flat photo at its original size. A JPEG saved as JPG keeps the bytes exactly as
/// the service sent them.
fn download_photo(client: &Client, info: &PanoInfo, link: &str, options: &Options, control: &Control) -> Result<Saved, Error> {
    (control.progress)(Step::Tiles(0, 1));
    let (w, h) = info.full_size();
    let mut urls = if w > 0 { info.whole_image_urls(w, h) } else { Vec::new() };
    if let Some(base) = &info.image_url {
        urls.push(format!("{base}=s0"));
    }
    let mut best: Option<(RgbImage, Vec<u8>)> = None;
    let mut last = Error::Service(service::Error::NotFound);
    for url in urls {
        match fetch(client, &url, 2, control.cancel).and_then(|bytes| Ok((decode(&bytes)?, bytes))) {
            Ok((image, bytes)) => {
                let larger = best.as_ref().map_or(true, |(b, _)| image.width() * image.height() > b.width() * b.height());
                let enough = w > 0 && image.width() as f64 >= w as f64 * 0.98 && image.height() as f64 >= h as f64 * 0.98;
                if larger {
                    best = Some((image, bytes));
                }
                if enough {
                    break;
                }
            }
            Err(e @ (Error::Cancelled | Error::Service(service::Error::Network(_)))) => return Err(e),
            Err(e) => last = e,
        }
    }
    let (image, bytes) = best.ok_or(last)?;
    (control.progress)(Step::Tiles(1, 1));
    control.check()?;
    (control.progress)(Step::Saving);
    let name = file_stem(info);
    let path = options.out_dir.join(format!("{name}.{}", options.format.extension()));
    if options.format == Format::Jpg && is_jpeg(&bytes) {
        save_raw(&bytes, &path)?;
    } else {
        save(&image, &path, options.format)?;
    }
    if options.json {
        let data = sidecar(info, link, image.dimensions(), Touches::default());
        fs::write(options.out_dir.join(format!("{name}.json")), serde_json::to_string_pretty(&data).unwrap_or_default())?;
    }
    Ok(Saved { path, size: image.dimensions(), missing_tiles: 0 })
}

/// The frame of a crop: where it looks and its proportions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crop {
    pub view: View,
    /// Width divided by height.
    pub aspect: f64,
}

impl Crop {
    /// A crop from the interface's numbers: its viewer turns left for a positive yaw and measures
    /// the field of view vertically, in degrees.
    pub fn from_viewer(yaw: f64, pitch: f64, vertical_fov_deg: f64, aspect: f64) -> Crop {
        let half = (vertical_fov_deg.to_radians() / 2.0).tan();
        Crop { view: View { yaw: -yaw, pitch, hfov: 2.0 * (half * aspect).atan() }, aspect }
    }

    /// The vertical field of view, in degrees.
    pub fn vertical_fov_deg(&self) -> f64 {
        (2.0 * ((self.view.hfov / 2.0).tan() / self.aspect).atan()).to_degrees()
    }

    /// The size the photo will have when taken from a panorama `pano_width` pixels wide: the same
    /// detail as the panorama at its centre, and no side longer than [`MAX_CROP_SIDE`].
    pub fn size(&self, pano_width: u32) -> (u32, u32) {
        let w = geom::max_crop_width(pano_width, self.view.hfov) as f64;
        let h = w / self.aspect;
        let scale = (MAX_CROP_SIDE as f64 / w.max(h)).min(1.0);
        (((w * scale).round() as u32).max(16), ((h * scale).round() as u32).max(16))
    }

    /// The end of a crop's file name: where it looks (compass-like angle from the centre of the
    /// panorama, elevation) and how wide, so crops of the same panorama can be told apart.
    pub fn name_suffix(&self) -> String {
        let azimuth = self.view.yaw.to_degrees().rem_euclid(360.0).round() as i64 % 360;
        let elevation = self.view.pitch.to_degrees().round() as i64;
        format!("crop_{azimuth:03}_{elevation:+03}_{}", self.vertical_fov_deg().round() as i64)
    }
}

/// Takes an ordinary photo of the framed part out of a whole panorama (as the camera took it) and
/// writes it, at the highest resolution the panorama allows.
pub fn extract_crop(image: &RgbImage, info: &PanoInfo, options: &Options, crop: &Crop) -> Result<Saved, Error> {
    let completed;
    let image = if extras::is_partial(image.width(), image.height()) && options.complete {
        completed = extras::complete_sphere(image);
        completed.as_ref().unwrap_or(image)
    } else {
        image
    };
    let level = if options.level { level_of(info) } else { None };
    let (w, h) = crop.size(image.width());
    let photo = geom::perspective(image, &crop.view, w, h, level.as_ref(), Quality::Best);
    let name = format!("{}_{}", file_stem(info), crop.name_suffix());
    let path = free_name(&options.out_dir, &name, options.format.extension());
    save(&photo, &path, options.format)?;
    Ok(Saved { path, size: photo.dimensions(), missing_tiles: 0 })
}

/// `name.ext`, or `name_2.ext`, `name_3.ext`... when that exists: crops never replace each other.
fn free_name(dir: &Path, name: &str, extension: &str) -> PathBuf {
    let first = dir.join(format!("{name}.{extension}"));
    if !first.exists() {
        return first;
    }
    (2..10_000)
        .map(|n| dir.join(format!("{name}_{n}.{extension}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> PanoInfo {
        PanoInfo {
            id: "AAAAAAAAAAAAAAAAAAAAAA".into(),
            kind: Kind::Official,
            flat: false,
            sizes: vec![(512, 256), (1024, 512), (2048, 1024)],
            tile: (512, 512),
            lat: Some(40.416734),
            lon: Some(-3.703331),
            heading: Some(271.0),
            tilt: Some(90.0),
            roll: Some(0.0),
            date: Some((2024, 11, None)),
            author: None,
            copyright: None,
            address: Some("Example Square / Sampleton".into()),
            history: vec![],
            image_url: None,
        }
    }

    #[test]
    fn names_tell_versions_apart() {
        let info = info();
        assert_eq!(file_stem(&info), "street_2024-11_40.41673_-3.70333_AAAAAAAAAAAA");
        let mut options = Options::default();
        assert_eq!(suffixes(&info, &options, 2), "");
        options.north = true;
        options.cover_ground = true;
        assert_eq!(suffixes(&info, &options, 1), "_1024px_nadir_north");
    }

    #[test]
    fn user_uploads_and_photos_are_named_by_the_end_of_their_id() {
        let mut info = info();
        info.kind = Kind::User;
        info.id = "CIHM0ogKEICAgICPqPujeQ".into();
        info.date = Some((2023, 2, Some(14)));
        assert_eq!(file_stem(&info), "photo360_2023-02-14_40.41673_-3.70333_CAgICPqPujeQ");
        info.flat = true;
        info.lat = None;
        assert_eq!(file_stem(&info), "photo_2023-02-14_CAgICPqPujeQ");
    }

    #[test]
    fn tiles_are_placed_and_clipped() {
        let mut canvas = RgbImage::new(6, 4);
        let tile = RgbImage::from_pixel(4, 4, image::Rgb([9, 9, 9]));
        blit(&mut canvas, &tile, 4, 2);
        assert_eq!(canvas.get_pixel(3, 3)[0], 0);
        assert_eq!(canvas.get_pixel(4, 2)[0], 9);
        assert_eq!(canvas.get_pixel(5, 3)[0], 9);
        assert_eq!(canvas.get_pixel(4, 1)[0], 0);
    }

    #[test]
    fn every_format_can_be_written_and_read_back() {
        let dir = std::env::temp_dir().join(format!("openview-test-{}", std::process::id()));
        let image = RgbImage::from_fn(64, 32, |x, y| image::Rgb([x as u8 * 4, y as u8 * 8, 128]));
        for format in Format::ALL {
            let path = dir.join(format!("sample.{}", format.extension()));
            save(&image, &path, format).expect("saved");
            let back = image::open(&path).expect("readable").into_rgb8();
            assert_eq!(back.dimensions(), (64, 32));
            let worst = image.as_raw().iter().zip(back.as_raw()).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
            assert!(worst <= if format == Format::Jpg { 6 } else { 0 }, "{format:?}: {worst}");
            assert!(!dir.join(format!("sample.{}.part", format.extension())).exists());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_large_flat_tiff_is_compressed() {
        let dir = std::env::temp_dir().join(format!("openview-tiff-{}", std::process::id()));
        let image = RgbImage::from_pixel(512, 256, image::Rgb([40, 90, 200]));
        let path = dir.join("flat.tif");
        save(&image, &path, Format::Tif).expect("saved");
        let size = fs::metadata(&path).unwrap().len();
        assert!(size < 512 * 256 * 3 / 10, "the TIFF is {size} bytes: not compressed");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn original_bytes_are_kept_as_they_are() {
        let dir = std::env::temp_dir().join(format!("openview-raw-{}", std::process::id()));
        let bytes = [0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];
        assert!(is_jpeg(&bytes) && !is_jpeg(b"\x89PNG"));
        let path = dir.join("p.jpg");
        save_raw(&bytes, &path).expect("saved");
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn crops_never_replace_each_other() {
        let dir = std::env::temp_dir().join(format!("openview-names-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(free_name(&dir, "a_crop", "jpg"), dir.join("a_crop.jpg"));
        fs::write(dir.join("a_crop.jpg"), b"x").unwrap();
        assert_eq!(free_name(&dir, "a_crop", "jpg"), dir.join("a_crop_2.jpg"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn crop_size_follows_the_frame() {
        let crop = Crop { view: View { yaw: 0.0, pitch: 0.0, hfov: 90f64.to_radians() }, aspect: 16.0 / 9.0 };
        assert_eq!(crop.size(16384), (5215, 2933));
        // A very wide frame is limited by its longest side.
        let wide = Crop { view: View { yaw: 0.0, pitch: 0.0, hfov: 170f64.to_radians() }, aspect: 2.0 };
        assert_eq!(wide.size(16384), (20000, 10000));
    }

    #[test]
    fn the_interface_s_crop_numbers_are_understood() {
        // Its viewer: positive yaw looks left, the field of view is vertical and in degrees.
        let crop = Crop::from_viewer(0.5, 0.2, 60.0, 1.5);
        assert!((crop.view.yaw + 0.5).abs() < 1e-12 && (crop.view.pitch - 0.2).abs() < 1e-12);
        assert!((crop.vertical_fov_deg() - 60.0).abs() < 1e-9);
        // Same size as the original app computes: height = 2 f tan(fov / 2), width = height * aspect.
        let f = 16384.0 / std::f64::consts::TAU;
        let h = 2.0 * f * (30f64.to_radians()).tan();
        assert_eq!(crop.size(16384), ((h * 1.5).round() as u32, h.round() as u32));
        assert_eq!(crop.name_suffix(), "crop_331_+11_60");
    }

    #[test]
    fn a_crop_is_cut_from_a_whole_panorama() {
        let dir = std::env::temp_dir().join(format!("openview-crop-{}", std::process::id()));
        let pano = RgbImage::from_fn(1024, 512, |x, _| if (480..544).contains(&x) { image::Rgb([250, 250, 250]) } else { image::Rgb([10, 10, 10]) });
        let options = Options { out_dir: dir.clone(), format: Format::Png, ..Options::default() };
        let crop = Crop::from_viewer(0.0, 0.0, 40.0, 1.0);
        let saved = extract_crop(&pano, &info(), &options, &crop).expect("cropped");
        assert_eq!(saved.size, crop.size(1024));
        let photo = image::open(&saved.path).unwrap().into_rgb8();
        let centre = photo.get_pixel(photo.width() / 2, photo.height() / 2);
        assert!(centre[0] > 200, "the middle of the view is not the middle of the crop: {centre:?}");
        assert!(photo.get_pixel(2, photo.height() / 2)[0] < 60);
        assert!(saved.path.file_name().unwrap().to_string_lossy().ends_with("_crop_000_+00_40.png"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn data_file_describes_the_panorama() {
        let info = info();
        let touches = Touches { north: true, levelled: true, ..Touches::default() };
        let data = sidecar(&info, "https://example.invalid/link", (2048, 1024), touches);
        assert_eq!(data["heading"], 271.0);
        assert_eq!(data["heading_of_centre"], 0.0);
        assert_eq!(data["width"], 2048);
        assert_eq!(data["address"], "Example Square / Sampleton");
        assert_eq!(data["link"], "https://example.invalid/link");
        assert_eq!((data["levelled"].as_bool(), data["ground_covered"].as_bool()), (Some(true), Some(false)));
        assert!(sidecar(&info, "", (1, 1), touches)["link"].as_str().unwrap().contains("!1sAAAAAAAAAAAAAAAAAAAAAA!2e0"));
    }
}
