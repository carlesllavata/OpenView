# Changelog

What changed for users in each version of OpenView, newest first.

## 0.2.0 — 13 November 2026

OpenView is now based on the original OpenView by Daniel Castellanos: it has its look
and its logo, and works the way it does.

- **New window.** The interface of the original OpenView: cards with a large preview, the kind, the
  place, the resolution, the date and the author; a bar at the bottom with **Download all**; short
  notices when a file is saved; **Clear finished**; a 360 viewer that fills the window.
- **English and Spanish.** A button at the top switches the language; at first it follows Windows.
  The theme (light or dark) also follows Windows until you choose one.
- **Ordinary photos are recognised by themselves**, whatever link you paste, are shown larger with
  **View photo**, and are saved at their original size exactly as stored.
- **360 photos of users download in one piece**, faster, and are kept exactly as stored when no
  option changes them.
- **Cards show the name of the place** when the map service has one.
- **Create HDRI works like the original OpenView's**: new **Sky and highlight boost** and **Overall
  exposure** controls and **Reset values**; the sun size is now its radius (1° at first) and its
  strength starts at 5; below the preview, the sun's height, how much it was raised and the dynamic
  range. EXR files are about half the size, and a conversion shows its progress and can be
  cancelled.
- **Crops** are named after where they look (`_crop_<direction>_<height>_<field of view>`) and can
  be up to 20,000 pixels on their longest side.
- **TIFF files are compressed** (LZW, without loss) and PNG files are smaller.
- **Clearer messages** when there is no connection, the folder cannot be written, the disk is full
  or the service does not have a panorama.
- The window needs WebView2, the web engine built into Windows; without it OpenView opens in your
  browser.
- Command line: `OpenView hdri` takes `--highlights` and `--exposure`; `--sun` is now the ratio to
  the sky (5 by default) and `--sun-size` the radius (1° by default). `--window <image>` opens the
  window in the HDRI converter.
- The options of 0.1.0 are not carried over: the folder and the options start from their defaults
  once.
- The Open3D launcher (0.3.0 or later) installs, updates and removes OpenView; the README explains
  both ways to install it.

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
