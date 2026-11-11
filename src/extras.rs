// SPDX-License-Identifier: GPL-3.0-or-later
//! Touch-ups of a downloaded panorama: turning it to face north, covering the ground under the
//! camera and completing 360 photos that do not cover the whole sphere.

use image::RgbImage;
use rayon::prelude::*;

/// How far around the point under the camera the ground patch reaches, in degrees.
pub const NADIR_ANGLE_DEG: f64 = 27.0;

/// Rolls the image sideways: column `x` of the result is column `x + shift` of the source.
pub fn shift_x(image: &RgbImage, shift: i64) -> RgbImage {
    let (w, h) = (image.width() as usize, image.height() as usize);
    let offset = shift.rem_euclid(w.max(1) as i64) as usize * 3;
    if offset == 0 {
        return image.clone();
    }
    let mut out = vec![0u8; w * h * 3];
    out.par_chunks_mut(w * 3).zip(image.as_raw().par_chunks(w * 3)).for_each(|(dst, src)| {
        dst[..w * 3 - offset].copy_from_slice(&src[offset..]);
        dst[w * 3 - offset..].copy_from_slice(&src[..offset]);
    });
    RgbImage::from_raw(w as u32, h as u32, out).expect("buffer has the right size")
}

/// The shift for [`shift_x`] that puts north in the centre of a panorama whose centre looks
/// towards `heading_deg`.
pub fn north_shift(width: u32, heading_deg: f64) -> i64 {
    -(heading_deg.rem_euclid(360.0) / 360.0 * width as f64).round() as i64
}

/// The centre of a panorama looks towards its heading; this turns it so the centre looks north.
pub fn face_north(image: &RgbImage, heading_deg: f64) -> RgbImage {
    shift_x(image, north_shift(image.width(), heading_deg))
}

/// One row of colours that can be averaged over any wrapped span quickly.
struct EdgeRow {
    /// Running sums, one more entry than pixels, per channel.
    sums: Vec<[f64; 3]>,
    w: usize,
}

impl EdgeRow {
    /// The average of `rows` consecutive rows of the image starting at `first`.
    fn from_rows(image: &RgbImage, first: usize, rows: usize) -> Self {
        let (w, h) = (image.width() as usize, image.height() as usize);
        let data = image.as_raw();
        let rows = rows.max(1).min(h - first.min(h - 1));
        let mut sums = vec![[0f64; 3]; w + 1];
        for x in 0..w {
            let mut pixel = [0f64; 3];
            for y in first..first + rows {
                let i = (y * w + x) * 3;
                for k in 0..3 {
                    pixel[k] += data[i + k] as f64;
                }
            }
            for k in 0..3 {
                sums[x + 1][k] = sums[x][k] + pixel[k] / rows as f64;
            }
        }
        EdgeRow { sums, w }
    }

    /// Average colour of the columns from `x - radius` to `x + radius`, wrapping around.
    fn average(&self, x: usize, radius: usize) -> [f32; 3] {
        let w = self.w;
        let total = self.sums[w];
        if radius * 2 + 1 >= w {
            return [(total[0] / w as f64) as f32, (total[1] / w as f64) as f32, (total[2] / w as f64) as f32];
        }
        let span = (radius * 2 + 1) as f64;
        let start = (x + w - radius) % w;
        let end = start + radius * 2 + 1;
        let mut out = [0f32; 3];
        for k in 0..3 {
            let sum = if end <= w {
                self.sums[end][k] - self.sums[start][k]
            } else {
                (total[k] - self.sums[start][k]) + self.sums[end - w][k]
            };
            out[k] = (sum / span) as f32;
        }
        out
    }
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Blur radius for a row that is `t` (0 to 1) of the way from the picture's edge to the pole: at
/// the pole every column has the same colour, as it must on a sphere.
fn spread(w: usize, t: f32) -> usize {
    2 + ((w / 2) as f32 * t.clamp(0.0, 1.0).powf(1.6)) as usize
}

/// Rows at the top and at the bottom that hold no picture (all black).
pub fn black_bands(image: &RgbImage) -> (usize, usize) {
    let (w, h) = (image.width() as usize, image.height() as usize);
    let data = image.as_raw();
    let is_black = |y: usize| data[y * w * 3..(y + 1) * w * 3].iter().step_by(7).all(|v| *v < 6);
    let top = (0..h).take_while(|y| is_black(*y)).count();
    if top == h {
        return (0, 0);
    }
    let bottom = (0..h).rev().take_while(|y| is_black(*y)).count();
    (top, bottom)
}

/// Cuts away the black that pads a picture on its right and bottom sides, as in the smallest tile
/// of a panorama, where the picture sits in the top-left corner.
pub fn trim_padding(image: &RgbImage) -> RgbImage {
    let (w, h) = (image.width() as usize, image.height() as usize);
    let data = image.as_raw();
    let dark = |x: usize, y: usize| data[(y * w + x) * 3..(y * w + x) * 3 + 3].iter().all(|v| *v < 10);
    let mut keep_h = h;
    while keep_h > 1 && (0..w).step_by(3).all(|x| dark(x, keep_h - 1)) {
        keep_h -= 1;
    }
    let mut keep_w = w;
    while keep_w > 1 && (0..keep_h).step_by(3).all(|y| dark(keep_w - 1, y)) {
        keep_w -= 1;
    }
    if keep_w == w && keep_h == h {
        return image.clone();
    }
    image::imageops::crop_imm(image, 0, 0, keep_w as u32, keep_h as u32).to_image()
}

/// Fills `top` rows at the top and `bottom` rows at the bottom by stretching the nearest row of
/// the picture and blurring it more and more towards the pole.
pub fn fill_bands(image: &mut RgbImage, top: usize, bottom: usize) {
    let (w, h) = (image.width() as usize, image.height() as usize);
    if top + bottom >= h || (top == 0 && bottom == 0) {
        return;
    }
    let sample = ((h - top - bottom) / 60).clamp(1, 12);
    let top_edge = EdgeRow::from_rows(image, top, sample);
    let bottom_edge = EdgeRow::from_rows(image, h - bottom - sample, sample);
    let data: &mut [u8] = image.as_mut();
    data.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
        let (edge, t) = if y < top {
            (&top_edge, (top - y) as f32 / top as f32)
        } else if y >= h - bottom {
            (&bottom_edge, (y + 1 - (h - bottom)) as f32 / bottom as f32)
        } else {
            return;
        };
        let radius = spread(w, t);
        for x in 0..w {
            let c = edge.average(x, radius);
            row[x * 3] = (c[0] + 0.5) as u8;
            row[x * 3 + 1] = (c[1] + 0.5) as u8;
            row[x * 3 + 2] = (c[2] + 0.5) as u8;
        }
    });
}

/// Completes a 360 photo that does not cover the whole sphere: the picture is centred on the
/// horizon of a 2:1 image and the missing sky and ground are filled from its edges. Images that
/// already are 2:1 only get their black bands filled. Returns `None` when nothing had to change.
pub fn complete_sphere(image: &RgbImage) -> Option<RgbImage> {
    let (w, h) = (image.width() as usize, image.height() as usize);
    let full_h = w / 2;
    if h > full_h || w < 8 {
        return None;
    }
    let pad_top = (full_h - h) / 2;
    let mut out = if h == full_h {
        image.clone()
    } else {
        let mut out = RgbImage::new(w as u32, full_h as u32);
        let start = pad_top * w * 3;
        let dst: &mut [u8] = out.as_mut();
        dst[start..start + w * h * 3].copy_from_slice(image.as_raw());
        out
    };
    let (top, bottom) = black_bands(&out);
    if top == 0 && bottom == 0 {
        return (h != full_h).then_some(out);
    }
    fill_bands(&mut out, top, bottom);
    Some(out)
}

/// Covers what is right under the camera (tripod, person, the car's blur) with a patch of the
/// colour of the ground around it. A patch, not a reconstruction.
pub fn patch_nadir(image: &mut RgbImage, angle_deg: f64) {
    let (w, h) = (image.width() as usize, image.height() as usize);
    let rows = ((angle_deg / 180.0) * h as f64).round() as usize;
    if rows < 4 || rows + 8 >= h {
        return;
    }
    let start = h - rows;
    let feather = (rows / 6).max(2) as f32;
    let edge = EdgeRow::from_rows(image, start.saturating_sub(6), 6);
    let data: &mut [u8] = image.as_mut();
    data.par_chunks_mut(w * 3).enumerate().skip(start).for_each(|(y, row)| {
        let depth = (y - start) as f32;
        let radius = spread(w, 0.02 + depth / rows as f32);
        let cover = smoothstep(0.0, feather, depth + 1.0);
        for x in 0..w {
            let c = edge.average(x, radius);
            for k in 0..3 {
                let old = row[x * 3 + k] as f32;
                row[x * 3 + k] = (old + (c[k] - old) * cover + 0.5) as u8;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    #[test]
    fn shifting_rolls_columns_around() {
        let image = RgbImage::from_fn(8, 2, |x, _| Rgb([x as u8, 0, 0]));
        let rolled = shift_x(&image, 3);
        let first_row: Vec<u8> = (0..8).map(|x| rolled.get_pixel(x, 0)[0]).collect();
        assert_eq!(first_row, vec![3, 4, 5, 6, 7, 0, 1, 2]);
        assert_eq!(shift_x(&image, -5), rolled);
        assert_eq!(shift_x(&image, 8), image);
    }

    #[test]
    fn north_ends_up_in_the_centre() {
        // The centre looks east (90), so north is a quarter turn to the left of the centre.
        let w = 360u32;
        let image = RgbImage::from_fn(w, 4, |x, _| if x == 90 { Rgb([255, 0, 0]) } else { Rgb([0, 0, 0]) });
        let turned = face_north(&image, 90.0);
        assert_eq!(turned.get_pixel(180, 0)[0], 255);
        assert_eq!(north_shift(w, 0.0), 0);
        assert_eq!(north_shift(w, 360.0), 0);
    }

    #[test]
    fn partial_photo_is_centred_and_filled() {
        // 400 wide, only 100 tall: the full sphere is 200 tall.
        let partial = RgbImage::from_fn(400, 100, |x, y| {
            if y < 50 {
                Rgb([100, 150, 250])
            } else {
                Rgb([(60 + x % 30) as u8, 90, 40])
            }
        });
        let full = complete_sphere(&partial).expect("completed");
        assert_eq!(full.dimensions(), (400, 200));
        // The picture itself is untouched, in the middle.
        assert_eq!(full.get_pixel(7, 50), partial.get_pixel(7, 0));
        assert_eq!(full.get_pixel(390, 149), partial.get_pixel(390, 99));
        // Sky above, ground below, and no black left.
        assert_eq!(*full.get_pixel(200, 0), Rgb([100, 150, 250]));
        let bottom = full.get_pixel(200, 199);
        assert!(bottom[1] == 90 && bottom[2] == 40 && (60..90).contains(&bottom[0]), "{bottom:?}");
        assert!(full.pixels().all(|p| p[0] > 0 || p[1] > 0 || p[2] > 0));
        // At the pole every column is the same colour.
        assert_eq!(full.get_pixel(0, 199), full.get_pixel(123, 199));
    }

    #[test]
    fn complete_spheres_are_left_alone() {
        let full = RgbImage::from_fn(64, 32, |x, y| Rgb([x as u8 + 10, y as u8 + 10, 99]));
        assert!(complete_sphere(&full).is_none());
    }

    #[test]
    fn black_bands_in_a_full_size_image_are_filled() {
        let banded = RgbImage::from_fn(64, 32, |_, y| if (8..24).contains(&y) { Rgb([200, 100, 50]) } else { Rgb([0, 0, 0]) });
        assert_eq!(black_bands(&banded), (8, 8));
        let full = complete_sphere(&banded).expect("filled");
        assert_eq!(*full.get_pixel(5, 0), Rgb([200, 100, 50]));
        assert_eq!(*full.get_pixel(60, 31), Rgb([200, 100, 50]));
    }

    #[test]
    fn padding_is_trimmed_from_the_right_and_the_bottom() {
        let padded = RgbImage::from_fn(64, 64, |x, y| if x < 60 && y < 30 { Rgb([90, 120, 200]) } else { Rgb([0, 0, 0]) });
        assert_eq!(trim_padding(&padded).dimensions(), (60, 30));
        let full = RgbImage::from_pixel(16, 8, Rgb([50, 50, 50]));
        assert_eq!(trim_padding(&full).dimensions(), (16, 8));
    }

    #[test]
    fn nadir_patch_only_touches_the_bottom() {
        let mut image = RgbImage::from_fn(360, 180, |x, y| {
            if y > 165 && (150..210).contains(&x) {
                Rgb([10, 10, 10]) // the tripod
            } else {
                Rgb([120, 110, 100])
            }
        });
        let before = image.clone();
        patch_nadir(&mut image, NADIR_ANGLE_DEG);
        let first_changed = 180 - 27;
        for y in 0..first_changed {
            assert_eq!(image.get_pixel(180, y), before.get_pixel(180, y), "row {y} changed");
        }
        let pole = image.get_pixel(180, 179);
        assert!(pole[0] > 110 && pole[1] > 100, "the tripod is still there: {pole:?}");
    }
}
