// SPDX-License-Identifier: GPL-3.0-or-later
//! Downloads a panorama: fetches its tiles, joins them, applies the touch-ups the user asked for
//! and writes the file.

use std::fs;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::codecs::tiff::TiffEncoder;
use image::{ImageEncoder, RgbImage};
use serde_json::json;

use crate::extras;
use crate::geom::{self, Mat3, Quality, View};
use crate::links::Kind;
use crate::service::{self, Client, PanoInfo, PhotoInfo};

const WORKERS: usize = 16;
const TRIES: u32 = 4;
pub const JPEG_QUALITY: u8 = 97;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

    pub fn label(self) -> &'static str {
        match self {
            Format::Jpg => "JPG",
            Format::Png => "PNG",
            Format::Tif => "TIFF",
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
        match self {
            Error::Cancelled => write!(f, "cancelled"),
            Error::Service(e) => write!(f, "{e}"),
            Error::Image(what) => write!(f, "image error: {what}"),
            Error::Io(e) => write!(f, "could not write the file: {e}"),
        }
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
        Error::Image(e.to_string())
    }
}

/// What a job is doing, for the progress bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    /// Tiles fetched so far, of how many.
    Tiles(usize, usize),
    Completing,
    Levelling,
    CoveringGround,
    Cropping,
    Saving,
}

impl Step {
    pub fn label(self) -> String {
        match self {
            Step::Tiles(done, total) => format!("Downloading {done}/{total}"),
            Step::Completing => "Completing the sphere".into(),
            Step::Levelling => "Levelling the horizon".into(),
            Step::CoveringGround => "Covering the ground".into(),
            Step::Cropping => "Extracting the crop".into(),
            Step::Saving => "Saving".into(),
        }
    }

    /// 0 to 1.
    pub fn fraction(self) -> f32 {
        match self {
            Step::Tiles(done, total) => 0.85 * done as f32 / total.max(1) as f32,
            Step::Completing => 0.86,
            Step::Levelling => 0.88,
            Step::CoveringGround => 0.92,
            Step::Cropping => 0.9,
            Step::Saving => 0.95,
        }
    }
}

/// The handles a running job needs: a way to be cancelled and a way to report progress.
pub struct Control<'a> {
    pub cancel: &'a AtomicBool,
    pub progress: &'a (dyn Fn(Step) + Sync),
}

impl Control<'_> {
    fn check(&self) -> Result<(), Error> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

fn decode(bytes: &[u8]) -> Result<RgbImage, Error> {
    Ok(image::load_from_memory(bytes)?.into_rgb8())
}

fn fetch_tile(client: &Client, info: &PanoInfo, zoom: usize, x: u32, y: u32, cancel: &AtomicBool) -> Result<Option<RgbImage>, Error> {
    let mut last = None;
    for attempt in 0..TRIES {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(400 * (1 << attempt)));
        }
        match client.tile(info, zoom, x, y) {
            Ok(bytes) => match decode(&bytes) {
                Ok(tile) => return Ok(Some(tile)),
                Err(e) => last = Some(e),
            },
            // The service has no tile here: the picture does not reach this far.
            Err(service::Error::NotFound) => return Ok(None),
            Err(e) => last = Some(Error::Service(e)),
        }
    }
    Err(last.unwrap_or(Error::Image("tile".into())))
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
pub fn fetch_image(client: &Client, info: &PanoInfo, zoom: usize, control: &Control) -> Result<(RgbImage, usize), Error> {
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
                match fetch_tile(client, info, zoom, x, y, control.cancel) {
                    Ok(Some(tile)) => {
                        let (need_w, need_h) = (tw.min(w - x * tw), th.min(h - y * th));
                        let tile = if tile.width() >= need_w && tile.height() >= need_h {
                            tile
                        } else {
                            // Some tiles arrive at a lower resolution than announced.
                            image::imageops::resize(&tile, tw, th, image::imageops::FilterType::CatmullRom)
                        };
                        blit(&mut canvas.lock().unwrap(), &tile, x * tw, y * th);
                    }
                    Ok(None) => {
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

/// A small version of the panorama for the card and the viewer: the largest zoom level that is
/// not wider than `max_width`.
pub fn fetch_preview(client: &Client, info: &PanoInfo, max_width: u32, cancel: &AtomicBool) -> Result<RgbImage, Error> {
    let control = Control { cancel, progress: &|_| {} };
    let zoom = info.zoom_for(Some(max_width));
    Ok(fetch_image(client, info, zoom, &control)?.0)
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

/// The file name (without suffixes or extension) of a panorama.
pub fn file_stem(info: &PanoInfo) -> String {
    let mut stem = String::from(match info.kind {
        Kind::Official => "street",
        Kind::User => "photo360",
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

/// Writes an image in the chosen format. The file appears under its final name only when it is
/// complete.
pub fn save(image: &RgbImage, path: &Path, format: Format) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let partial = path.with_extension(format!("{}.part", format.extension()));
    let result = (|| -> Result<(), Error> {
        let writer = BufWriter::with_capacity(1 << 20, fs::File::create(&partial)?);
        let (w, h) = image.dimensions();
        let colour = image::ExtendedColorType::Rgb8;
        match format {
            Format::Jpg => JpegEncoder::new_with_quality(writer, JPEG_QUALITY).write_image(image.as_raw(), w, h, colour)?,
            Format::Png => PngEncoder::new_with_quality(writer, CompressionType::Fast, FilterType::Adaptive)
                .write_image(image.as_raw(), w, h, colour)?,
            Format::Tif => TiffEncoder::new(writer).write_image(image.as_raw(), w, h, colour)?,
        }
        Ok(())
    })();
    match result {
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

/// The data file that can go with a panorama.
pub fn sidecar(info: &PanoInfo, options: &Options, size: (u32, u32), levelled: bool) -> serde_json::Value {
    let north = options.north && info.heading.is_some();
    json!({
        "id": info.id,
        "kind": match info.kind { Kind::Official => "street", Kind::User => "user" },
        "date": info.date_text(),
        "latitude": info.lat,
        "longitude": info.lon,
        "heading": info.heading,
        "heading_of_centre": if north { Some(0.0) } else { info.heading },
        "tilt": info.tilt,
        "roll": info.roll,
        "levelled": levelled,
        "width": size.0,
        "height": size.1,
        "author": info.author,
        "copyright": info.copyright,
        "link": info.map_link(),
        "app": format!("OpenView {}", crate::VERSION),
    })
}

/// The matrix that straightens this panorama, when there is something to straighten.
pub fn level_of(info: &PanoInfo) -> Option<Mat3> {
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

/// Downloads a panorama with the chosen options and writes it.
pub fn download(client: &Client, info: &PanoInfo, options: &Options, control: &Control) -> Result<Saved, Error> {
    let zoom = info.zoom_for(options.max_width);
    let (mut image, missing_tiles) = fetch_image(client, info, zoom, control)?;

    if options.complete && image.width() != image.height() * 2 {
        (control.progress)(Step::Completing);
        if let Some(full) = extras::complete_sphere(&image) {
            image = full;
        }
        control.check()?;
    }
    let spherical = image.width() == image.height() * 2;
    let mut levelled = false;
    if options.level && spherical {
        if let Some(matrix) = level_of(info) {
            (control.progress)(Step::Levelling);
            image = geom::reproject(&image, &matrix);
            levelled = true;
            control.check()?;
        }
    }
    if options.cover_ground && spherical {
        (control.progress)(Step::CoveringGround);
        extras::patch_nadir(&mut image, extras::NADIR_ANGLE_DEG);
    }
    if options.north {
        if let Some(heading) = info.heading {
            image = extras::face_north(&image, heading);
        }
    }
    control.check()?;

    (control.progress)(Step::Saving);
    let name = format!("{}{}", file_stem(info), suffixes(info, options, zoom));
    let path = options.out_dir.join(format!("{name}.{}", options.format.extension()));
    save(&image, &path, options.format)?;
    if options.json {
        let data = sidecar(info, options, image.dimensions(), levelled);
        fs::write(options.out_dir.join(format!("{name}.json")), serde_json::to_string_pretty(&data).unwrap_or_default())?;
    }
    Ok(Saved { path, size: image.dimensions(), missing_tiles })
}

/// The frame of a crop: where it looks and its proportions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crop {
    pub view: View,
    /// Width divided by height.
    pub aspect: f64,
}

impl Crop {
    /// The size the photo will have when taken from a panorama `pano_width` pixels wide.
    pub fn size(&self, pano_width: u32) -> (u32, u32) {
        let w = geom::max_crop_width(pano_width, self.view.hfov);
        (w, ((w as f64 / self.aspect).round() as u32).max(1))
    }
}

/// Downloads the whole panorama and writes an ordinary photo of the framed part, at the highest
/// resolution the panorama allows.
pub fn extract_crop(client: &Client, info: &PanoInfo, options: &Options, crop: &Crop, control: &Control) -> Result<Saved, Error> {
    let zoom = info.zoom_for(options.max_width);
    let (mut image, missing_tiles) = fetch_image(client, info, zoom, control)?;
    if options.complete && image.width() != image.height() * 2 {
        if let Some(full) = extras::complete_sphere(&image) {
            image = full;
        }
    }
    control.check()?;
    (control.progress)(Step::Cropping);
    let level = if options.level { level_of(info) } else { None };
    let (w, h) = crop.size(image.width());
    let photo = geom::perspective(&image, &crop.view, w, h, level.as_ref(), Quality::Best);
    drop(image);
    control.check()?;

    (control.progress)(Step::Saving);
    let stem = file_stem(info);
    let path = free_name(&options.out_dir, &format!("{stem}_crop"), options.format.extension());
    save(&photo, &path, options.format)?;
    Ok(Saved { path, size: photo.dimensions(), missing_tiles })
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

fn photo_extension(bytes: &[u8]) -> &'static str {
    match bytes {
        [0xFF, 0xD8, ..] => "jpg",
        [0x89, b'P', b'N', b'G', ..] => "png",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => "webp",
        [b'G', b'I', b'F', ..] => "gif",
        _ => "jpg",
    }
}

/// Downloads a flat photo at its original size and writes the bytes exactly as they arrive.
pub fn download_photo(client: &Client, photo: &PhotoInfo, out_dir: &Path, control: &Control) -> Result<Saved, Error> {
    (control.progress)(Step::Tiles(0, 1));
    let bytes = client.get_bytes(&photo.original_url())?;
    control.check()?;
    (control.progress)(Step::Saving);
    let size = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok())
        .unwrap_or((0, 0));
    fs::create_dir_all(out_dir)?;
    let path = out_dir.join(format!("photo_{}.{}", short_id(&photo.id), photo_extension(&bytes)));
    fs::write(&path, &bytes)?;
    Ok(Saved { path, size, missing_tiles: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> PanoInfo {
        PanoInfo {
            id: "AAAAAAAAAAAAAAAAAAAAAA".into(),
            kind: Kind::Official,
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
            history: vec![],
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
    fn user_uploads_are_named_by_the_end_of_their_id() {
        let mut info = info();
        info.kind = Kind::User;
        info.id = "CIHM0ogKEICAgICPqPujeQ".into();
        info.date = Some((2023, 2, Some(14)));
        assert_eq!(file_stem(&info), "photo360_2023-02-14_40.41673_-3.70333_CAgICPqPujeQ");
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
        }
        assert!(!dir.join("sample.jpg.part").exists());
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
    }

    #[test]
    fn photo_types_are_recognised() {
        assert_eq!(photo_extension(&[0xFF, 0xD8, 0xFF, 0xE0]), "jpg");
        assert_eq!(photo_extension(b"\x89PNG\r\n"), "png");
        assert_eq!(photo_extension(b"RIFF\0\0\0\0WEBPVP8 "), "webp");
    }

    #[test]
    fn data_file_describes_the_panorama() {
        let info = info();
        let options = Options { north: true, ..Options::default() };
        let data = sidecar(&info, &options, (2048, 1024), false);
        assert_eq!(data["heading"], 271.0);
        assert_eq!(data["heading_of_centre"], 0.0);
        assert_eq!(data["width"], 2048);
        assert!(data["link"].as_str().unwrap().contains("!1sAAAAAAAAAAAAAAAAAAAAAA!2e0"));
    }
}
