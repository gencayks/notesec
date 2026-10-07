//! User settings, stored as `config.toml` in the graph folder.
//!
//! ```toml
//! theme = "dark"          # or "light"
//! font_size = 16.0
//! font_family = "Inter"   # optional; omit to use the system UI font
//! ```
//!
//! The file is read once at startup. The app rewrites it when you change the
//! theme or font size (via shortcuts or the Ctrl-K palette), so hand edits to
//! other keys survive only if they are valid ones we know about.

use crate::storage::write_atomic;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const MIN_FONT_SIZE: f32 = 10.0;
pub const MAX_FONT_SIZE: f32 = 32.0;
pub const DEFAULT_FONT_SIZE: f32 = 16.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeKind {
    #[default]
    Dark,
    Light,
}

impl ThemeKind {
    pub fn toggled(self) -> Self {
        match self {
            ThemeKind::Dark => ThemeKind::Light,
            ThemeKind::Light => ThemeKind::Dark,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
// Any key missing from the file falls back to its default.
#[serde(default)]
pub struct Config {
    pub theme: ThemeKind,
    pub font_size: f32,
    /// `None` means "use the system UI font".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: ThemeKind::default(),
            font_size: DEFAULT_FONT_SIZE,
            font_family: None,
        }
    }
}

impl Config {
    pub fn path(root: &Path) -> PathBuf {
        root.join("config.toml")
    }

    /// Load settings from `<root>/config.toml`.
    ///
    /// A missing file gives the defaults. An unreadable or invalid file also
    /// gives the defaults, but first the bad file is copied to `config.toml.bak`
    /// so the next save cannot silently destroy someone's hand-written settings.
    pub fn load(root: &Path) -> Config {
        let path = Self::path(root);
        let Ok(text) = fs::read_to_string(&path) else {
            return Config::default();
        };
        match toml::from_str::<Config>(&text) {
            Ok(config) => config.sanitized(),
            Err(err) => {
                eprintln!("notesec: ignoring invalid {}: {err}", path.display());
                let backup = path.with_extension("toml.bak");
                if let Err(e) = fs::copy(&path, &backup) {
                    eprintln!("notesec: could not back up {}: {e}", path.display());
                }
                Config::default()
            }
        }
    }

    /// Clamp values into a usable range (a typo like `font_size = 0` or
    /// `1e9` must not make the UI unusable).
    fn sanitized(mut self) -> Self {
        self.font_size = self.clamp_size(self.font_size);
        // An empty family name means "unset".
        if self
            .font_family
            .as_deref()
            .is_some_and(|f| f.trim().is_empty())
        {
            self.font_family = None;
        }
        self
    }

    fn clamp_size(&self, size: f32) -> f32 {
        if size.is_finite() {
            size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
        } else {
            DEFAULT_FONT_SIZE
        }
    }

    /// Change the font size by `delta` points, staying within limits.
    pub fn adjust_font_size(&mut self, delta: f32) {
        self.font_size = self.clamp_size(self.font_size + delta);
    }

    /// Write settings to `<root>/config.toml` atomically.
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let body = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut text = String::from("# notesec settings\n");
        if self.font_family.is_none() {
            text.push_str("# font_family = \"Inter\"   # uncomment to pick a font\n");
        }
        text.push_str(&body);
        write_atomic(&Self::path(root), &text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("notesec-cfg-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = temp_dir("missing");
        assert_eq!(Config::load(&dir), Config::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn round_trip() {
        let dir = temp_dir("roundtrip");
        let config = Config {
            theme: ThemeKind::Light,
            font_size: 20.0,
            font_family: Some("Inter".into()),
        };
        config.save(&dir).unwrap();
        assert_eq!(Config::load(&dir), config);
        // No temp file left behind by the atomic write.
        assert!(!dir.join(".config.toml.tmp").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn default_save_keeps_a_font_hint_and_loads_back() {
        let dir = temp_dir("hint");
        Config::default().save(&dir).unwrap();
        let text = fs::read_to_string(Config::path(&dir)).unwrap();
        assert!(text.contains("# font_family"));
        assert_eq!(Config::load(&dir), Config::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn partial_files_fill_in_defaults() {
        let dir = temp_dir("partial");
        fs::write(Config::path(&dir), "theme = \"light\"\n").unwrap();
        let c = Config::load(&dir);
        assert_eq!(c.theme, ThemeKind::Light);
        assert_eq!(c.font_size, DEFAULT_FONT_SIZE);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn out_of_range_sizes_are_clamped() {
        let dir = temp_dir("clamp");
        fs::write(Config::path(&dir), "font_size = 0.5\n").unwrap();
        assert_eq!(Config::load(&dir).font_size, MIN_FONT_SIZE);
        fs::write(
            Config::path(&dir),
            "font_size = 1000.0\nfont_family = \"  \"\n",
        )
        .unwrap();
        let c = Config::load(&dir);
        assert_eq!(c.font_size, MAX_FONT_SIZE);
        assert_eq!(c.font_family, None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_file_falls_back_and_is_backed_up() {
        let dir = temp_dir("invalid");
        fs::write(Config::path(&dir), "theme = \"purple\"\n").unwrap();
        assert_eq!(Config::load(&dir), Config::default());
        let backup = fs::read_to_string(dir.join("config.toml.bak")).unwrap();
        assert_eq!(backup, "theme = \"purple\"\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn adjusting_font_size_stays_in_bounds() {
        let mut c = Config::default();
        c.adjust_font_size(2.0);
        assert_eq!(c.font_size, 18.0);
        c.adjust_font_size(1000.0);
        assert_eq!(c.font_size, MAX_FONT_SIZE);
        c.adjust_font_size(-1000.0);
        assert_eq!(c.font_size, MIN_FONT_SIZE);
    }
}
