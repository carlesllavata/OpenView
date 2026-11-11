# OpenView

Download 360 panoramas and photos from map applications at **full resolution**, look around them,
cut ordinary photos out of them and turn them into HDRIs. A single Windows program, no install, no
account. Part of the [Open3D](https://github.com/carlesllavata/Open3D) suite of open-source tools
for 3D work.

**Version 0.1.0** – in development, not released yet.

What it downloads:

- **Street-level panoramas** taken by the map service (the car with the camera): usually
  16384 × 8192 px; older ones 13312 × 6656.
- **360 photos uploaded by users** (the blue circles), at the resolution they were uploaded.
- **Ordinary photos of places**, at their original size and exactly as stored (not recompressed).

A panorama is saved as a standard **2:1 equirectangular** image, ready for any editor and for
3ds Max (a spherical environment map, a dome light, a sky).

## Install

Unzip `openview-<version>-windows-x64.zip` anywhere and run `OpenView.exe`. Nothing else is needed:
no Python, no runtime, no installer. To remove it, delete the folder and, if you want, the settings
in `%APPDATA%\OpenView`.

Windows 10 or 11, 64-bit. A 16K panorama needs about 1.5 GB of free memory while it is processed.

## The window

1. Copy the address of a panorama or a photo from the map application and press **Ctrl+V** anywhere
   in the window (or paste into the box and press **Add**). Several links at once are fine.
2. Each link becomes a card with a picture, the kind, the date, the resolution and the author.
3. Choose the folder, the format and the options at the top, and press **Download**. A download can
   be cancelled, and repeated with other options (**Download again**).

### Options

| Option | What it does |
|---|---|
| **Folder…** | Where files are saved (at first, `Pictures\OpenView`) |
| **JPG / PNG / TIFF** | JPG at quality 97, or PNG / TIFF without loss if the image will be edited a lot |
| **Level horizon** (on) | Straightens panoramas taken with a leaning camera, see below |
| **Centre to north** | Turns the image so that its centre looks north; saved with `_north` in the name |
| **Complete partials** (on) | Fills the missing sky and ground of 360 photos that do not cover the whole sphere |
| **Cover ground** | Covers what is right under the camera with a patch; saved with `_nadir` in the name |
| **Save data (.json)** | Also writes position, date, heading, author and the original link |

### Level horizon

Cameras are not always straight: the car leans on a slope or a bump. The service stores that lean
with every panorama; with **Level horizon** OpenView undoes it in the viewer, in crops and in the
downloaded image, so the horizon is level and the verticals of buildings are vertical, which is what
a 3D scene needs.

### Other dates

When the place has been captured more than once, the card shows a strip with every date and a small
picture. Click one to switch the card to that date (before the road works, in summer, without
cars…). Captures marked `user` are 360 photos uploaded by users near the same point.

### View in 360

Opens the panorama in a viewer: drag to look around, use the wheel to zoom.

### Extract crop

On the card or inside the viewer. A frame appears: choose its proportions (16:9, 3:2, 4:3, 1:1, 4:5,
9:16), aim by dragging and zoom with the wheel. The size the photo will have is shown at the bottom
all the time. **Extract** downloads the whole panorama and saves an ordinary photo (straight lines
stay straight) of what is inside the frame, at the highest resolution the panorama allows: the more
you zoom in, the narrower the view and the fewer pixels, because no detail is invented. Crops are
saved with `_crop` in the name and never replace each other.

### Cover ground and complete partials

- **Cover ground** hides the tripod, the person or the car's blur under the camera with a patch of
  the colour of the ground around it. It is a patch, not a reconstruction: fine when that spot is
  not seen in your scene.
- **Complete partials** centres a partial 360 photo on the horizon of a 2:1 image and fills the
  missing sky and ground by stretching and blurring its edges. Such photos are marked **Partial**
  on their card; they are rare.

### Create HDRI

Press **Create HDRI** on a downloaded panorama, or **Image to HDRI…** to convert any 360 image on
your computer (JPG, PNG or TIFF; for example one you retouched or removed the sky from: the
transparency of a PNG or TIFF is kept in the EXR).

An ordinary image does not keep the real intensity of the sun or the sky, so OpenView rebuilds it,
without AI: the image goes to linear values, the sky, clouds and other blown-out areas get their
range back, and a sun disc is added where the sun is, with the strength of a real day.

- **Sun strength**: the light the sun gives compared with the rest of the sky. Clear day 4–8, haze
  1–3, overcast 0.
- **Sun size**: smaller sun, sharper shadows.
- **Sun position**: found automatically (the centre of the largest blown-out patch of the sky). If
  it is wrong or missing, **click on the sun** in the preview. **No sun** is for overcast days.
- **Look with exposure / See the sun (−8 EV)**: only changes the preview. At −8 EV only the sun
  should be left shining.
- **Resolution** 2K, 4K, 8K or Original, and **EXR** (recommended) or **HDR**.

The HDRI is saved next to the image with `_hdri` at the end of the name. For lighting, 4K–8K is
plenty; the 16K JPG can stay as the sharp background.

### Other details

The window remembers its size and place, the theme (dark or light) and every option, in
`%APPDATA%\OpenView\settings.json`. If you close it while something is downloading, it asks first.
Everything runs on your computer: no account, no telemetry, nothing is sent anywhere except the
requests for the images themselves.

## File names

```
street_2025-05_40.41674_-3.70339_VV3xw9r4XbdA.jpg        kind, date, latitude, longitude, id
street_..._VV3xw9r4XbdA_nadir_north.jpg                  with Cover ground and Centre to north
street_..._VV3xw9r4XbdA_8192px.jpg                       a smaller version (--max-width)
street_..._VV3xw9r4XbdA_crop.jpg, _crop_2.jpg            crops
street_..._VV3xw9r4XbdA_hdri.exr                         HDRI
photo360_2024-11-15_....png                              a user's 360 photo
photo_AgICz2MKBtgE.jpg                                   an ordinary photo
```

## Command line

Without arguments the window opens. With arguments OpenView works from the terminal:

```
OpenView "<link>" -o "<folder>"
OpenView link1 link2 -o out -f png
OpenView --list links.txt -o out
OpenView --window "<link>"            open the window with the link already added
```

| Option | What it does |
|---|---|
| `-o`, `--out <folder>` | Destination folder (default: the current one) |
| `-f`, `--format jpg/png/tif` | `jpg` by default |
| `-n`, `--north` | Turn the image so its centre looks north |
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

`--sun-size <degrees>`, `--res original` and `-o <file>` also exist; `OpenView --help` lists
everything.

## Which link to copy

Open the panorama in the map application (enter the street view, the 360 photo or the photo) and
copy the address from the browser's address bar. A link that only has a position (`@lat,lon`)
downloads the newest street-level panorama within 50 m. Short "Share" links are followed too.

## Limits and notes

- OpenView uses internal services of a third party. They are not documented and may change without
  notice; if downloads stop working, look for a newer version.
- Downloading images this way may not be allowed by the terms of use of the service they come from,
  and the images belong to their authors. Use them for personal work or as reference, and do not
  redistribute them.
- Short "Share" links are implemented but have not been checked against real ones yet.
- The HDRI is an estimate made from an 8-bit image, good for lighting and reflections; it is not a
  measured HDRI.
- English only for now.

## Build from source

Rust (stable) on Windows:

```
cargo test                               unit tests
cargo test --test live -- --ignored      checks against the real service (needs the network)
cargo build --release                    the program, in target\release\OpenView.exe
python -I tools\package.py               tests, release build and dist\openview-<version>-windows-x64.zip
```

`cargo run --example make_icon` redraws `assets/icon.ico`; `cargo run --example verticals -- <id>…`
measures the levelling on real panoramas.

## License

OpenView is free software under the **GNU General Public License v3.0 or later**
(`GPL-3.0-or-later`); see [LICENSE](LICENSE). The libraries it is built with keep their own
licences, listed in `THIRD-PARTY-LICENSES.txt` inside the release zip.

## Credits

The idea and the list of features come from *Pano360 Downloader* by Adán Castellanos. OpenView is
written from scratch from the description of what that program does; it contains none of its code.
