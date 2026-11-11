// SPDX-License-Identifier: GPL-3.0-or-later
//! What the app remembers between runs: the options, the theme and the window's place.
//! Kept in `%APPDATA%\OpenView\settings.json`.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::hdri::HdrFormat;
use crate::pano::{Format, Options};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub out_dir: PathBuf,
    pub format: Format,
    pub north: bool,
    pub level: bool,
    pub cover_ground: bool,
    pub complete: bool,
    pub json: bool,
    pub dark: bool,
    /// Window position and size in points: x, y, width, height.
    pub window: Option<[f32; 4]>,
    /// Screen pixels per point when the window was saved.
    pub window_scale: f32,
    pub sun_strength: f32,
    pub sun_size_deg: f32,
    /// Width of the HDRI; `None` keeps the size of the image.
    pub hdri_width: Option<u32>,
    pub hdri_format: HdrFormat,
}

impl Default for Settings {
    fn default() -> Self {
        let options = Options::default();
        let hdri = crate::hdri::Params::default();
        Settings {
            out_dir: default_out_dir(),
            format: options.format,
            north: options.north,
            level: options.level,
            cover_ground: options.cover_ground,
            complete: options.complete,
            json: options.json,
            dark: true,
            window: None,
            window_scale: 1.0,
            sun_strength: hdri.sun_strength,
            sun_size_deg: hdri.sun_size_deg,
            hdri_width: Some(8192),
            hdri_format: HdrFormat::Exr,
        }
    }
}

fn default_out_dir() -> PathBuf {
    match std::env::var_os("USERPROFILE") {
        Some(home) => PathBuf::from(home).join("Pictures").join("OpenView"),
        None => PathBuf::from("."),
    }
}

/// The folder the app keeps its data in.
pub fn data_dir() -> PathBuf {
    match std::env::var_os("APPDATA") {
        Some(appdata) => PathBuf::from(appdata).join("OpenView"),
        None => PathBuf::from(".").join("OpenView"),
    }
}

impl Settings {
    pub fn file() -> PathBuf {
        data_dir().join("settings.json")
    }

    /// The saved settings, or the defaults when there are none or they cannot be read.
    pub fn load() -> Settings {
        fs::read_to_string(Self::file()).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        let file = Self::file();
        if let Some(dir) = file.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = fs::write(file, text);
        }
    }

    /// The download options these settings stand for.
    pub fn options(&self) -> Options {
        Options {
            out_dir: self.out_dir.clone(),
            format: self.format,
            north: self.north,
            level: self.level,
            cover_ground: self.cover_ground,
            complete: self.complete,
            json: self.json,
            max_width: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_survive_a_round_trip() {
        let mut settings = Settings::default();
        settings.format = Format::Tif;
        settings.north = true;
        settings.window = Some([10.0, 20.0, 900.0, 700.0]);
        settings.hdri_width = None;
        let text = serde_json::to_string(&settings).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&text).unwrap(), settings);
    }

    #[test]
    fn missing_and_unknown_fields_fall_back_to_defaults() {
        let settings: Settings = serde_json::from_str(r#"{"north": true, "from_the_future": 1}"#).unwrap();
        assert!(settings.north && settings.level && settings.complete);
        assert_eq!(settings.format, Format::Jpg);
    }

    #[test]
    fn options_follow_the_settings() {
        let mut settings = Settings::default();
        settings.cover_ground = true;
        settings.level = false;
        let options = settings.options();
        assert!(options.cover_ground && !options.level && options.max_width.is_none());
    }
}
