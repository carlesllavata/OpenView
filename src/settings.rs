// SPDX-License-Identifier: GPL-3.0-or-later
//! What the app remembers between runs: the download options, the theme, the language and the
//! window's place. Kept in `%APPDATA%\OpenView\settings.json`, with the same keys the interface
//! uses.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::pano::{Format, Options};

/// Where the window was, in screen pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowPlace {
    pub w: u32,
    pub h: u32,
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub max: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub outdir: PathBuf,
    pub fmt: Format,
    pub north: bool,
    pub json: bool,
    /// Cover the ground under the camera.
    pub nadir: bool,
    /// Complete partial 360 photos to 2:1.
    pub fill: bool,
    /// Level the horizon.
    pub level: bool,
    /// "", "light" or "dark" ("" follows Windows).
    pub theme: String,
    /// "", "en" or "es" ("" follows Windows).
    pub lang: String,
    pub window: Option<WindowPlace>,
}

impl Default for Settings {
    fn default() -> Self {
        let options = Options::default();
        Settings {
            outdir: default_out_dir(),
            fmt: options.format,
            north: options.north,
            json: options.json,
            nadir: options.cover_ground,
            fill: options.complete,
            level: options.level,
            theme: String::new(),
            lang: String::new(),
            window: None,
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
            out_dir: self.outdir.clone(),
            format: self.fmt,
            north: self.north,
            level: self.level,
            cover_ground: self.nadir,
            complete: self.fill,
            json: self.json,
            max_width: None,
        }
    }

    /// Applies what the page sends: only known keys, only with the right type and value. The
    /// window's place and the folder are not the page's to set (the folder is chosen in a dialog).
    pub fn apply_patch(&mut self, patch: &Value) {
        let Some(patch) = patch.as_object() else { return };
        let mut current = serde_json::to_value(&*self).unwrap_or(Value::Null);
        let Some(fields) = current.as_object_mut() else { return };
        for (key, value) in patch {
            if matches!(key.as_str(), "window" | "outdir") {
                continue;
            }
            let Some(slot) = fields.get_mut(key) else { continue };
            let same_type = matches!((&*slot, value), (Value::Bool(_), Value::Bool(_)) | (Value::String(_), Value::String(_)));
            let allowed = match key.as_str() {
                "fmt" => matches!(value.as_str(), Some("jpg" | "png" | "tif")),
                "theme" => matches!(value.as_str(), Some("" | "light" | "dark")),
                "lang" => matches!(value.as_str(), Some("" | "en" | "es")),
                _ => true,
            };
            if same_type && allowed {
                *slot = value.clone();
            }
        }
        if let Ok(updated) = serde_json::from_value(current) {
            *self = updated;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn settings_survive_a_round_trip() {
        let mut settings = Settings::default();
        settings.fmt = Format::Tif;
        settings.north = true;
        settings.window = Some(WindowPlace { w: 900, h: 700, x: Some(10), y: None, max: false });
        let text = serde_json::to_string(&settings).unwrap();
        assert!(text.contains(r#""fmt":"tif""#), "{text}");
        assert_eq!(serde_json::from_str::<Settings>(&text).unwrap(), settings);
    }

    #[test]
    fn missing_and_unknown_fields_fall_back_to_defaults() {
        let settings: Settings = serde_json::from_str(r#"{"north": true, "from_the_future": 1}"#).unwrap();
        assert!(settings.north && settings.level && settings.fill && !settings.nadir);
        assert_eq!((settings.fmt, settings.theme.as_str()), (Format::Jpg, ""));
    }

    #[test]
    fn the_page_can_only_change_what_it_should() {
        let mut settings = Settings::default();
        let before = settings.clone();
        settings.apply_patch(&json!({
            "north": true, "fmt": "png", "theme": "dark", "lang": "es",
            "level": "yes", "json": 1, "outdir": "C:/elsewhere", "window": {"w": 1, "h": 1, "max": true},
            "unknown": true
        }));
        assert!(settings.north && settings.level && !settings.json);
        assert_eq!((settings.fmt, settings.theme.as_str(), settings.lang.as_str()), (Format::Png, "dark", "es"));
        assert_eq!((settings.outdir.clone(), settings.window), (before.outdir, None));
        settings.apply_patch(&json!({"fmt": "bmp", "theme": "pink", "lang": "fr"}));
        assert_eq!((settings.fmt, settings.theme.as_str(), settings.lang.as_str()), (Format::Png, "dark", "es"));
    }

    #[test]
    fn options_follow_the_settings() {
        let mut settings = Settings::default();
        settings.nadir = true;
        settings.level = false;
        let options = settings.options();
        assert!(options.cover_ground && !options.level && options.max_width.is_none());
    }
}
