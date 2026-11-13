// SPDX-License-Identifier: GPL-3.0-or-later
//! Geometry of 360 images: directions, rotations, levelling and perspective views.
//!
//! An equirectangular image maps longitude to x (centre of the image = longitude 0, growing to the
//! right) and latitude to y (top = +90 degrees). Directions are unit vectors with x to the right,
//! y up and z towards the centre of the image.

use std::f64::consts::{PI, TAU};

use image::RgbImage;
use rayon::prelude::*;

pub type Vec3 = [f64; 3];
pub type Mat3 = [[f64; 3]; 3];

pub const IDENTITY: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

pub fn mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut out = [[0.0; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j];
        }
    }
    out
}

pub fn apply(m: &Mat3, v: Vec3) -> Vec3 {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

pub fn transpose(m: &Mat3) -> Mat3 {
    let mut out = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            out[i][j] = m[j][i];
        }
    }
    out
}

/// Rotation about x: a positive angle turns the forward direction downwards.
pub fn rot_x(a: f64) -> Mat3 {
    let (s, c) = a.sin_cos();
    [[1.0, 0.0, 0.0], [0.0, c, -s], [0.0, s, c]]
}

/// Rotation about y: a positive angle turns the forward direction to the right.
pub fn rot_y(a: f64) -> Mat3 {
    let (s, c) = a.sin_cos();
    [[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]]
}

/// Rotation about z (the forward direction): a positive angle turns "right" towards "up".
pub fn rot_z(a: f64) -> Mat3 {
    let (s, c) = a.sin_cos();
    [[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]]
}

pub fn dir_from_lon_lat(lon: f64, lat: f64) -> Vec3 {
    let (sl, cl) = lat.sin_cos();
    [cl * lon.sin(), sl, cl * lon.cos()]
}

pub fn lon_lat_from_dir(d: Vec3) -> (f64, f64) {
    let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(1e-12);
    (d[0].atan2(d[2]), (d[1] / len).clamp(-1.0, 1.0).asin())
}

/// How far the camera was from level, in degrees: (pitch, roll). The service stores a tilt of 90
/// and a roll of 0 (or 360) for a level camera.
pub fn camera_lean(tilt_deg: f64, roll_deg: f64) -> (f64, f64) {
    let mut roll = roll_deg % 360.0;
    if roll > 180.0 {
        roll -= 360.0;
    } else if roll < -180.0 {
        roll += 360.0;
    }
    (tilt_deg - 90.0, roll)
}

/// The rotation that takes a direction of the level world to the camera's own frame, built from
/// explicit signs. [`level_matrix`] is the one to use; this exists so the signs can be measured.
pub fn lean_matrix(pitch_deg: f64, roll_deg: f64, pitch_sign: f64, roll_sign: f64) -> Mat3 {
    mul(&rot_z((roll_sign * roll_deg).to_radians()), &rot_x((pitch_sign * pitch_deg).to_radians()))
}

// Signs found by measuring how vertical the buildings end up on real panoramas (see
// `examples/verticals.rs`).
const PITCH_SIGN: f64 = 1.0;
const ROLL_SIGN: f64 = 1.0;

/// The rotation that straightens a panorama taken with a leaning camera, or `None` when the camera
/// was level enough for it not to matter.
pub fn level_matrix(tilt_deg: f64, roll_deg: f64) -> Option<Mat3> {
    let (pitch, roll) = camera_lean(tilt_deg, roll_deg);
    if pitch.abs() <= 0.05 && roll.abs() <= 0.05 {
        return None;
    }
    // A wild value is a broken record, not a camera on its side: better to touch nothing.
    if pitch.abs() > 25.0 || roll.abs() > 25.0 {
        return None;
    }
    Some(lean_matrix(pitch, roll, PITCH_SIGN, ROLL_SIGN))
}

/// The same rotation as [`level_matrix`] for the 360 viewer of the interface, whose forward
/// direction is -z instead of +z: flipping z on both sides of the matrix changes the sign of the
/// entries that mix z with x or y.
pub fn viewer_matrix(m: &Mat3) -> Mat3 {
    [[m[0][0], m[0][1], -m[0][2]], [m[1][0], m[1][1], -m[1][2]], [-m[2][0], -m[2][1], m[2][2]]]
}

/// Reads colours from an equirectangular image at any direction.
pub struct Sampler<'a> {
    data: &'a [u8],
    w: usize,
    h: usize,
}

fn cubic_weights(t: f32) -> [f32; 4] {
    let t2 = t * t;
    let t3 = t2 * t;
    [
        -0.5 * t3 + t2 - 0.5 * t,
        1.5 * t3 - 2.5 * t2 + 1.0,
        -1.5 * t3 + 2.0 * t2 + 0.5 * t,
        0.5 * t3 - 0.5 * t2,
    ]
}

impl<'a> Sampler<'a> {
    pub fn new(image: &'a RgbImage) -> Self {
        Sampler { data: image.as_raw(), w: image.width() as usize, h: image.height() as usize }
    }

    /// Continuous pixel position of a direction (pixel centres are at .5).
    pub fn position(&self, d: Vec3) -> (f64, f64) {
        let (lon, lat) = lon_lat_from_dir(d);
        ((lon / TAU + 0.5) * self.w as f64, (0.5 - lat / PI) * self.h as f64)
    }

    #[inline]
    fn pixel(&self, x: isize, y: isize) -> [f32; 3] {
        let x = x.rem_euclid(self.w as isize) as usize;
        let y = y.clamp(0, self.h as isize - 1) as usize;
        let i = (y * self.w + x) * 3;
        [self.data[i] as f32, self.data[i + 1] as f32, self.data[i + 2] as f32]
    }

    pub fn bilinear(&self, fx: f64, fy: f64) -> [u8; 3] {
        let (u, v) = (fx - 0.5, fy - 0.5);
        let (x0, y0) = (u.floor(), v.floor());
        let (tx, ty) = ((u - x0) as f32, (v - y0) as f32);
        let (x0, y0) = (x0 as isize, y0 as isize);
        let (a, b, c, d) =
            (self.pixel(x0, y0), self.pixel(x0 + 1, y0), self.pixel(x0, y0 + 1), self.pixel(x0 + 1, y0 + 1));
        let mut out = [0u8; 3];
        for k in 0..3 {
            let top = a[k] + (b[k] - a[k]) * tx;
            let bottom = c[k] + (d[k] - c[k]) * tx;
            out[k] = (top + (bottom - top) * ty + 0.5) as u8;
        }
        out
    }

    pub fn bicubic(&self, fx: f64, fy: f64) -> [u8; 3] {
        let (u, v) = (fx - 0.5, fy - 0.5);
        let (x0, y0) = (u.floor(), v.floor());
        let wx = cubic_weights((u - x0) as f32);
        let wy = cubic_weights((v - y0) as f32);
        let (x0, y0) = (x0 as isize, y0 as isize);
        let mut acc = [0f32; 3];
        for (j, wyj) in wy.iter().enumerate() {
            let mut row = [0f32; 3];
            for (i, wxi) in wx.iter().enumerate() {
                let p = self.pixel(x0 + i as isize - 1, y0 + j as isize - 1);
                row[0] += p[0] * wxi;
                row[1] += p[1] * wxi;
                row[2] += p[2] * wxi;
            }
            acc[0] += row[0] * wyj;
            acc[1] += row[1] * wyj;
            acc[2] += row[2] * wyj;
        }
        [
            (acc[0] + 0.5).clamp(0.0, 255.0) as u8,
            (acc[1] + 0.5).clamp(0.0, 255.0) as u8,
            (acc[2] + 0.5).clamp(0.0, 255.0) as u8,
        ]
    }
}

/// Rewrites a whole panorama through a rotation: every direction `d` of the result shows what the
/// source has at `m * d`.
pub fn reproject(src: &RgbImage, m: &Mat3) -> RgbImage {
    let (w, h) = (src.width() as usize, src.height() as usize);
    let sampler = Sampler::new(src);
    let columns: Vec<(f64, f64)> =
        (0..w).map(|x| (((x as f64 + 0.5) / w as f64 - 0.5) * TAU).sin_cos()).collect();
    let mut out = vec![0u8; w * h * 3];
    out.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
        let (sl, cl) = ((0.5 - (y as f64 + 0.5) / h as f64) * PI).sin_cos();
        for (x, (sin_lon, cos_lon)) in columns.iter().enumerate() {
            let d = apply(m, [cl * sin_lon, sl, cl * cos_lon]);
            let (fx, fy) = sampler.position(d);
            row[x * 3..x * 3 + 3].copy_from_slice(&sampler.bicubic(fx, fy));
        }
    });
    RgbImage::from_raw(w as u32, h as u32, out).expect("buffer has the right size")
}

/// Where a perspective view looks, in radians. `hfov` is the horizontal field of view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub yaw: f64,
    pub pitch: f64,
    pub hfov: f64,
}

impl View {
    /// The rotation from the view's own frame to the panorama's.
    pub fn matrix(&self) -> Mat3 {
        mul(&rot_y(self.yaw), &rot_x(-self.pitch))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    /// For the interactive viewer.
    Fast,
    /// For files.
    Best,
}

/// Renders an ordinary photo (straight lines stay straight) out of a panorama. `level` is the
/// matrix from [`level_matrix`], when the panorama has to be straightened on the way.
pub fn perspective(
    src: &RgbImage,
    view: &View,
    out_w: u32,
    out_h: u32,
    level: Option<&Mat3>,
    quality: Quality,
) -> RgbImage {
    let (w, h) = (out_w.max(1) as usize, out_h.max(1) as usize);
    let sampler = Sampler::new(src);
    let focal = (w as f64 / 2.0) / (view.hfov / 2.0).tan();
    let m = match level {
        Some(level) => mul(level, &view.matrix()),
        None => view.matrix(),
    };
    let mut out = vec![0u8; w * h * 3];
    out.par_chunks_mut(w * 3).enumerate().for_each(|(j, row)| {
        let y = -(j as f64 + 0.5 - h as f64 / 2.0);
        for i in 0..w {
            let x = i as f64 + 0.5 - w as f64 / 2.0;
            let d = apply(&m, [x, y, focal]);
            let (fx, fy) = sampler.position(d);
            let colour = match quality {
                Quality::Fast => sampler.bilinear(fx, fy),
                Quality::Best => sampler.bicubic(fx, fy),
            };
            row[i * 3..i * 3 + 3].copy_from_slice(&colour);
        }
    });
    RgbImage::from_raw(w as u32, h as u32, out).expect("buffer has the right size")
}

/// The widest a perspective view can be without inventing pixels: at its centre it has the same
/// detail as a panorama `src_width` pixels wide.
pub fn max_crop_width(src_width: u32, hfov: f64) -> u32 {
    let width = 2.0 * (hfov / 2.0).tan() * src_width as f64 / TAU;
    width.round().clamp(16.0, 1.0e6) as u32
}

/// Where a direction of the panorama falls in a view, in pixels from the view's top-left corner.
/// `None` when it is behind the camera.
pub fn project(view: &View, out_w: f64, out_h: f64, dir: Vec3) -> Option<(f64, f64)> {
    let local = apply(&transpose(&view.matrix()), dir);
    if local[2] <= 1e-6 {
        return None;
    }
    let focal = (out_w / 2.0) / (view.hfov / 2.0).tan();
    Some((out_w / 2.0 + local[0] / local[2] * focal, out_h / 2.0 - local[1] / local[2] * focal))
}

/// The direction of the panorama under a point of a view.
pub fn unproject(view: &View, out_w: f64, out_h: f64, x: f64, y: f64) -> Vec3 {
    let focal = (out_w / 2.0) / (view.hfov / 2.0).tan();
    let d = apply(&view.matrix(), [x - out_w / 2.0, -(y - out_h / 2.0), focal]);
    let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    [d[0] / len, d[1] / len, d[2] / len]
}

/// How far from vertical the vertical edges near the horizon are, on average, in degrees. In a
/// level panorama the verticals of buildings are exactly vertical lines, so a lower number means a
/// straighter panorama. Meant for comparing versions of the same image.
pub fn vertical_deviation(image: &RgbImage) -> f64 {
    let width = image.width().min(4096);
    let height = (width as u64 * image.height() as u64 / image.width().max(1) as u64) as u32;
    let small = image::imageops::resize(image, width, height.max(8), image::imageops::FilterType::Triangle);
    let (w, h) = (small.width() as usize, small.height() as usize);
    let grey: Vec<f32> = small
        .as_raw()
        .chunks_exact(3)
        .map(|p| 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32)
        .collect();
    let at = |x: usize, y: usize| grey[y * w + x % w];
    // Rows within 25 degrees of the horizon.
    let (first, last) = ((h as f64 * (65.0 / 180.0)) as usize, (h as f64 * (115.0 / 180.0)) as usize);
    let (first, last) = (first.max(1), last.min(h - 1));
    let sums: (f64, f64) = (first..last)
        .into_par_iter()
        .map(|y| {
            let (mut weighted, mut total) = (0f64, 0f64);
            for x in 0..w {
                let (l, r) = ((x + w - 1) % w, x + 1);
                let gx = (at(r, y - 1) + 2.0 * at(r, y) + at(r, y + 1))
                    - (at(l, y - 1) + 2.0 * at(l, y) + at(l, y + 1));
                let gy = (at(l, y + 1) + 2.0 * at(x, y + 1) + at(r, y + 1))
                    - (at(l, y - 1) + 2.0 * at(x, y - 1) + at(r, y - 1));
                let magnitude = (gx * gx + gy * gy).sqrt();
                if magnitude < 120.0 || gx.abs() < 1e-3 {
                    continue;
                }
                let angle = (gy / gx).atan().to_degrees().abs();
                if angle < 15.0 {
                    weighted += (magnitude * angle) as f64;
                    total += magnitude as f64;
                }
            }
            (weighted, total)
        })
        .reduce(|| (0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));
    if sums.1 > 0.0 {
        sums.0 / sums.1
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A panorama with a white spot at a known direction on a dark gradient.
    fn spotted(w: u32, lon_deg: f64, lat_deg: f64) -> RgbImage {
        let h = w / 2;
        let target = dir_from_lon_lat(lon_deg.to_radians(), lat_deg.to_radians());
        RgbImage::from_fn(w, h, |x, y| {
            let lon = ((x as f64 + 0.5) / w as f64 - 0.5) * TAU;
            let lat = (0.5 - (y as f64 + 0.5) / h as f64) * PI;
            let d = dir_from_lon_lat(lon, lat);
            let cos = d[0] * target[0] + d[1] * target[1] + d[2] * target[2];
            if cos > (3.0f64).to_radians().cos() {
                image::Rgb([255, 255, 255])
            } else {
                image::Rgb([(x % 64) as u8, 20, 40])
            }
        })
    }

    fn brightest(image: &RgbImage) -> (f64, f64) {
        let (mut sx, mut sy, mut n) = (0f64, 0f64, 0f64);
        for (x, y, p) in image.enumerate_pixels() {
            if p[0] > 200 && p[1] > 200 {
                sx += x as f64 + 0.5;
                sy += y as f64 + 0.5;
                n += 1.0;
            }
        }
        assert!(n > 0.0, "no bright spot in the image");
        (sx / n, sy / n)
    }

    #[test]
    fn directions_round_trip() {
        for (lon, lat) in [(0.0, 0.0), (1.0, 0.5), (-2.5, -1.2), (3.0, 1.4)] {
            let (lon2, lat2) = lon_lat_from_dir(dir_from_lon_lat(lon, lat));
            assert!((lon - lon2).abs() < 1e-9 && (lat - lat2).abs() < 1e-9);
        }
    }

    #[test]
    fn identity_reprojection_keeps_the_image() {
        let src = spotted(256, 40.0, 20.0);
        let out = reproject(&src, &IDENTITY);
        let worst = src.as_raw().iter().zip(out.as_raw()).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
        assert!(worst <= 1, "worst difference {worst}");
    }

    #[test]
    fn a_view_aimed_at_the_spot_has_it_in_the_middle() {
        let src = spotted(1024, 30.0, 10.0);
        let view = View { yaw: 30f64.to_radians(), pitch: 10f64.to_radians(), hfov: 60f64.to_radians() };
        let out = perspective(&src, &view, 400, 300, None, Quality::Best);
        let (x, y) = brightest(&out);
        assert!((x - 200.0).abs() < 1.5 && (y - 150.0).abs() < 1.5, "spot at {x},{y}");
    }

    #[test]
    fn a_spot_above_and_to_the_right_appears_there() {
        let src = spotted(1024, 10.0, 8.0);
        let view = View { yaw: 0.0, pitch: 0.0, hfov: 60f64.to_radians() };
        let out = perspective(&src, &view, 400, 300, None, Quality::Fast);
        let (x, y) = brightest(&out);
        assert!(x > 220.0 && y < 130.0, "spot at {x},{y}");
        let dir = dir_from_lon_lat(10f64.to_radians(), 8f64.to_radians());
        let (px, py) = project(&view, 400.0, 300.0, dir).expect("in front");
        assert!((px - x).abs() < 1.5 && (py - y).abs() < 1.5, "projected {px},{py} seen {x},{y}");
        let back = unproject(&view, 400.0, 300.0, px, py);
        assert!((back[0] - dir[0]).abs() < 1e-9 && (back[1] - dir[1]).abs() < 1e-9);
    }

    #[test]
    fn levelling_undoes_a_lean() {
        // Lean a level panorama with the inverse rotation, then level it: the spot comes back.
        let level = spotted(512, -50.0, 15.0);
        let m = lean_matrix(4.0, -3.0, PITCH_SIGN, ROLL_SIGN);
        let leaning = reproject(&level, &transpose(&m));
        let fixed = reproject(&leaning, &m);
        let (x0, y0) = brightest(&level);
        let (x1, y1) = brightest(&leaning);
        let (x2, y2) = brightest(&fixed);
        assert!((x0 - x1).abs() + (y0 - y1).abs() > 2.0, "the lean moved nothing");
        assert!((x0 - x2).abs() < 1.0 && (y0 - y2).abs() < 1.0, "{x0},{y0} became {x2},{y2}");
    }

    #[test]
    fn level_cameras_need_no_matrix() {
        assert!(level_matrix(90.0, 0.0).is_none());
        assert!(level_matrix(90.01, 359.99).is_none());
        assert!(level_matrix(92.0, 358.5).is_some());
        assert!(level_matrix(170.0, 0.0).is_none());
        assert!(level_matrix(90.0, 30.0).is_none());
        assert_eq!(camera_lean(92.0, 358.5), (2.0, -1.5));
    }

    #[test]
    fn the_viewer_matrix_is_the_same_rotation_seen_with_z_flipped() {
        let m = level_matrix(93.0, 2.0).expect("a lean");
        let v = viewer_matrix(&m);
        let d = [0.3, -0.2, 0.9];
        let ours = apply(&m, d);
        let theirs = apply(&v, [d[0], d[1], -d[2]]);
        assert!((ours[0] - theirs[0]).abs() < 1e-12 && (ours[1] - theirs[1]).abs() < 1e-12);
        assert!((ours[2] + theirs[2]).abs() < 1e-12);
        // The original app builds it as rz(roll) * rx(-tilt) in its own frame.
        let (t, r) = ((-3.0f64).to_radians(), 2.0f64.to_radians());
        let rx = [[1.0, 0.0, 0.0], [0.0, t.cos(), -t.sin()], [0.0, t.sin(), t.cos()]];
        let rz = [[r.cos(), -r.sin(), 0.0], [r.sin(), r.cos(), 0.0], [0.0, 0.0, 1.0]];
        let reference = mul(&rz, &rx);
        for i in 0..3 {
            for j in 0..3 {
                assert!((v[i][j] - reference[i][j]).abs() < 1e-12, "entry {i},{j}");
            }
        }
    }

    #[test]
    fn crop_width_follows_the_field_of_view() {
        let wide = max_crop_width(16384, 90f64.to_radians());
        let narrow = max_crop_width(16384, 30f64.to_radians());
        assert_eq!(wide, 5215);
        assert!(narrow < wide / 3);
    }

    #[test]
    fn straight_columns_measure_straighter_than_slanted_ones() {
        let bars = |slant: f64| {
            RgbImage::from_fn(1024, 512, |x, y| {
                // Smooth bars: hard ones are staircases of perfectly vertical steps.
                let shifted = x as f64 + slant * (y as f64 - 256.0);
                let v = (128.0 + 110.0 * (shifted / 4.0).sin()) as u8;
                image::Rgb([v, v, v])
            })
        };
        let straight = vertical_deviation(&bars(0.0));
        let slanted = vertical_deviation(&bars(0.1));
        assert!(straight < 0.5, "straight bars measured {straight}");
        assert!(slanted > straight + 2.0, "slanted {slanted} straight {straight}");
    }
}
