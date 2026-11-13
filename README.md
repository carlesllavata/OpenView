# OpenView

Download 360 panoramas and photos from map applications at **full resolution**, look around them,
cut ordinary photos out of them and turn them into HDRIs. A single Windows program, no install, no
account. Part of the [Open3D](https://github.com/carlesllavata/Open3D) suite of open-source tools
for 3D work, based on the original [OpenView](https://github.com/AdanCastellanos/OpenView) by Daniel
Castellanos (see [Credits](#credits)).

**Version 0.2.0** – based on the original OpenView: its interface, its logo and its methods.

What it downloads:

- **Street-level panoramas** taken by the map service (the car with the camera): usually
  16384 × 8192 px; older ones 13312 × 6656.
- **360 photos uploaded by users** (the blue circles), at the resolution they were uploaded.
- **Ordinary photos of places** (the ones you cannot save with a right click), at their original
  size and exactly as stored (not recompressed).

A panorama is saved as a standard **2:1 equirectangular** image, ready for any editor and for
3ds Max (a spherical environment map, a dome light, a sky).

## Install

Two ways, and nothing else is needed (no Python, no runtime):

- **With the [Open3D launcher](https://github.com/carlesllavata/Open3D)**, version 0.3.0 or later:
  press *Install* on OpenView's card. It goes in `%LOCALAPPDATA%\Programs\Open3D\OpenView`, with
  shortcuts in the Start menu and on the desktop, and the launcher updates it and removes it.
- **By hand**: download `openview-<version>-windows-x64.zip` from the
  [releases page](https://github.com/carlesllavata/OpenView/releases/latest), unzip it anywhere and
  run `OpenView.exe`. To remove it, delete the folder.

Removing OpenView either way keeps its settings in `%APPDATA%\OpenView`; delete that folder too if
you want them gone.

The program is not code-signed, so the first time Windows may show "Windows protected your PC":
choose *More info* and then *Run anyway*. `SHA256SUMS.txt` on the release page lets you check the
download.

Windows 10 or 11, 64-bit. The window uses WebView2, the web engine built into Windows (it comes with
Windows 11 and with current Windows 10). It does not sign in to any account, does not sync anything
and shares no history with your browser. If WebView2 is not available, OpenView opens in your
default browser instead. A 16K panorama needs about 1.5 GB of free memory while it is processed.

## The window

1. Copy the address of a panorama or a photo from the map application and press **Ctrl+V** anywhere
   in the window (or paste it into the box at the top and press **Add**). Several links at once are
   fine.
2. Each link becomes a card with a preview, the kind (street view, user 360, photo), the place, the
   resolution, the date and the author. **View in 360** lets you look around to check it is the
   right panorama; on a photo, **View photo** shows it larger.
3. Choose the folder, the format and the options, and press **Download** on a card, or **Download
   all** at the bottom. A download can be **cancelled**, and **Again** repeats it with other options
   (for example as TIFF). **Clear finished** empties the list of what is done.

The buttons at the top right switch the language (English or Spanish; at first it follows Windows)
and the theme (light or dark; at first it follows Windows).

### Options

| Option | What it does |
|---|---|
| **Save to** | Where files are saved (at first, `Pictures\OpenView`); the folder button opens it |
| **JPG / PNG / TIFF** | JPG at quality 97, or PNG / TIFF without loss if the image will be edited a lot |
| **Centre to north** | Turns the image so that its centre faces north; saved with `_north` in the name |
| **Save data (.json)** | Also writes position, place, date, heading, author and the original link |
| **Cover the ground** | Covers what is right under the camera with a patch; saved with `_nadir` in the name |
| **Complete partials** (on) | Fills the missing sky and ground of 360 photos that do not cover the whole sphere |
| **Level horizon** (on) | Straightens panoramas taken with a leaning camera, see below |

### Level horizon

Cameras are not always straight: the car leans on a slope or a bump. The service stores that lean
with every panorama; with **Level horizon** OpenView undoes it in the 360 viewer, in crops and in
the downloaded image, so the horizon is level and the verticals of buildings are vertical, which is
what a 3D scene needs.

### Other dates

When the place has been captured more than once, the card shows a strip with every date and a small
picture. Click one to switch the card to that date before downloading (before the road works, in
summer, without cars…).

### Extract crop

On the card or inside the 360 viewer. A frame appears: choose its proportions (16:9, 3:2, 4:3, 1:1,
4:5, 9:16), aim by dragging and zoom with the wheel. The size in pixels the photo will have is shown
all the time. **Extract** downloads the whole panorama and saves an ordinary photo (straight lines
stay straight) of what is inside the frame, at the highest resolution the panorama allows: the more
you zoom in, the narrower the view and the fewer pixels, because no detail is invented. Crops are
saved in the chosen format with `_crop` in the name, followed by the direction, the height above the
horizon and the field of view, and never replace each other.

### Cover the ground and complete partials

- **Cover the ground** hides the tripod, the person or the car's smudge under the camera with a
  patch of the colour of the ground around it. It is a patch, not a reconstruction: fine when that
  spot is not seen in your scene.
- **Complete partials** centres a partial 360 photo on the horizon of a 2:1 image and fills the
  missing sky and ground by stretching and blurring its edges. Such photos are marked **Partial**
  on their card; they are rare.

### Create HDRI

Press **Create HDRI** on a downloaded panorama, or **Image to HDRI…** at the top to convert any 360
image on your computer (JPG, PNG or TIFF; for example one you retouched or removed the sky from:
the transparency of a PNG or TIFF is kept in the EXR).

An ordinary image does not keep the real intensity of the sun or the sky, so OpenView rebuilds it,
without AI: the image goes to linear values, the sky, clouds and reflections get their brightness
back, and the sun is found and given the strength of a real day. Controls:

- **Sun strength**: the light of the sun compared with the light of the sky. Clear 4–8, haze 1–3,
  overcast 0. If the sun is not found or is wrong, **click on it** in the preview.
- **Sun size**: smaller, sharper shadows.
- **Sky and highlight boost**: how much brightness the clouds, the sky and the reflections get back.
- **Overall exposure**, in stops.
- **Look with exposure / See the sun (−8 EV)**: only changes the preview; at −8 EV only the sun
  should be left shining. Below the preview: the sun's height, how much it was raised and the
  dynamic range of the result.
- **Resolution** 2K / 4K / 8K / Original and **EXR** (recommended) or **HDR**. **Reset values**
  goes back to the defaults.

The HDRI is saved next to the image with `_hdri` at the end of the name; a conversion can be
cancelled. For lighting, 4K–8K is plenty; the 16K JPG can stay as the sharp background.

### Other details

If you close the window with downloads running, it asks first. The window remembers its size and
place, the language, the theme and every option, in `%APPDATA%\OpenView\settings.json`. Everything
runs on your computer: no account, no telemetry, nothing is sent anywhere except the requests for
the images themselves.

## File names

```
street_2025-05_40.41674_-3.70339_VV3xw9r4XbdA.jpg          kind, date, latitude, longitude, id
street_..._VV3xw9r4XbdA_nadir_north.jpg                    with Cover the ground and Centre to north
street_..._VV3xw9r4XbdA_8192px.jpg                         a smaller version (--max-width)
street_..._VV3xw9r4XbdA_crop_331_+11_60.jpg, ..._2.jpg     crops: direction, height, field of view
street_..._VV3xw9r4XbdA_hdri.exr                           HDRI
photo360_2024-11-15_....jpg                                a user's 360 photo
photo_2024-04-24_40.41703_-3.70274_AgICz2MKBtgE.jpg        an ordinary photo
```

## Command line

Without arguments the window opens. With arguments OpenView works from the terminal:

```
OpenView "<link>" -o "<folder>"
OpenView link1 link2 -o out -f png
OpenView --list links.txt -o out
OpenView --window "<link>"            open the window with the link already added
OpenView --window panorama.jpg        open the window in the HDRI converter with that image
```

| Option | What it does |
|---|---|
| `-o`, `--out <folder>` | Destination folder (default: the current one) |
| `-f`, `--format jpg/png/tif` | `jpg` by default |
| `-n`, `--north` | Turn the image so its centre faces north |
| `-l`, `--list <file>` | One link per line; lines starting with `#` are ignored |
| `-j`, `--json` | Also write the `.json` with the data |
| `--max-width <px>` | Download a smaller version, not wider than this |
| `--cover-ground` | Cover the ground under the camera |
| `--no-level` | Do not straighten the horizon |
| `--no-complete` | Do not complete partial 360 photos |

And to convert to HDRI:

```
OpenView hdri panorama.jpg                         8K EXR next to the image
OpenView hdri panorama.jpg --sun 8 --res 4096 --format hdr
OpenView hdri panorama.jpg --no-sun                overcast day
OpenView hdri panorama.jpg --sun-at 0.31,0.22      the sun is there (0–1 across and down)
```

`--sun-size <degrees>` (radius, default 1), `--highlights <x>` (default 3), `--exposure <stops>`,
`--res original` and `-o <file>` also exist; `OpenView --help` lists everything.

## Which link to copy

Open the panorama in the map application (enter the street view, the 360 photo or the photo) and
copy the address from the browser's address bar. A link that only has a position (`@lat,lon`)
downloads the newest street-level panorama within 50 m. Short links of the *Share* button are followed
too (see the limits below).

## Limits and notes

- OpenView uses internal services of a third party. They are not documented and may change without
  notice; if downloads stop working, look for a newer version.
- Downloading images this way may not be allowed by the terms of use of the service they come from,
  and the images belong to their authors. Use them for personal work or as reference, and do not
  redistribute them.
- Short *Share* links are implemented but have not been checked against real ones yet.
- The HDRI is an estimate made from an 8-bit image, good for lighting and reflections; it is not a
  measured HDRI.

## Build from source

Rust (stable) on Windows:

```
cargo test                               unit tests
cargo test --test live -- --ignored      checks against the real service (needs the network)
cargo build --release                    the program, in target\release\OpenView.exe
python -I tools\package.py               tests, release build and dist\openview-<version>-windows-x64.zip
```

The interface is `ui/index.html` with its texts in `ui/i18n/en.json` and `ui/i18n/es.json`; both are
built into the program. `cargo run --example verticals -- <id>…` measures the levelling on real
panoramas.

## License

OpenView is free software under the **GNU General Public License v3.0 or later**
(`GPL-3.0-or-later`); see [LICENSE](LICENSE). It includes code of the original OpenView, which is
under the MIT License: see [NOTICE](NOTICE) and [LICENSES](LICENSES). The libraries it is built with
keep their own licences, listed in `THIRD-PARTY-LICENSES.txt` inside the release zip.

## Credits

OpenView is based on the original [OpenView](https://github.com/AdanCastellanos/OpenView) by Daniel
Castellanos (MIT License): its interface and its logo are used as they are, and its download, crop
and HDRI methods were ported from Python to Rust. [NOTICE](NOTICE) lists what comes from it.
