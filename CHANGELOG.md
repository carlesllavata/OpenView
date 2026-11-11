# Changelog

What changed for users in each version of OpenView, newest first.

## 0.1.0 — 11 November 2026

The first version.

- Download street-level panoramas (up to 16384 × 8192), 360 photos uploaded by users and ordinary
  photos of places from a pasted link, at full resolution, as JPG, PNG or TIFF.
- Paste links with Ctrl+V anywhere in the window; each one becomes a card with its picture, kind,
  date, resolution and author. Downloads can be cancelled and repeated with other options.
- Other dates of the same place: a strip of dates with pictures on each card; click one to switch.
- Level horizon: panoramas taken with a leaning camera are straightened in the viewer, in crops and
  in the downloaded image.
- View in 360: look around a panorama before downloading it.
- Extract crop: frame a part of the panorama (16:9, 3:2, 4:3, 1:1, 4:5, 9:16) and save it as an
  ordinary photo at the highest resolution the panorama allows.
- Centre to north, cover the ground under the camera, complete 360 photos that do not cover the
  whole sphere, and an optional `.json` with position, date, heading, author and link.
- Create HDRI from a downloaded panorama or from any 360 image on disk: automatic sun detection
  (or click on it), sun strength and size, exposure preview, 2K to original size, EXR or HDR.
  Transparency of PNG and TIFF images is kept in the EXR.
- Command line for downloads and for HDRI conversion, without the window.
- Dark and light themes; the window remembers its place and every option.
