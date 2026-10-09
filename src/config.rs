//! User settings, stored as `config.toml` in the graph folder.
//!
//! ```toml

//! font_size = 16.0
//! font_family = "Inter"   # optional; omit to use the system UI font
//! git_backup = false      # commit the graph folder to local git (decision 39)
//! # AI (decision 42). The mode is always the user's explicit choice:
//! ai_provider = "local"    # "local" (default), "api" or "off"; never falls back
//! ai_endpoint = "http://localhost:1234/v1"  # Local: loopback http only
//! ai_model = ""            # Local chat model; empty picks the server's first one
//! ai_embedding_model = ""  # Local embedding model; empty: the chat model (decision 43)
//! ai_api_base = "https://api.openai.com/v1"  # API key mode: https base URL
//! ai_api_model = ""        # API key mode: model id (required by most providers)
//! ai_api_embedding_model = ""  # API key mode embeddings; empty: ai_api_model
//! vim_mode = false        # vim keybindings in the block editor (decision 48)
//! web_clipper = false     # the local web clipper endpoint (decision 51)
//! clipper_port = 27183    # its port on 127.0.0.1 (1024-65535)
//! # Voice notes (decision 52, docs/VOICE_NOTES.md):
//! voice_recorder = []      # e.g. ["arecord", "-D", "hw:1", "-f", "S16_LE", "-r", "16000", "-c", "1", "{file}"]
//! whisper_binary = ""      # whisper.cpp's whisper-cli; empty: no transcription
//! whisper_model = ""       # a ggml model file, e.g. ggml-base.bin
//! whisper_language = "auto"
//! voice_auto_transcribe = false
//! ```
//!
//! The API key itself is NOT stored here: it lives in `state.toml` as
//! `ai_api_key` (plaintext; see `state.rs`), so this hand-edited settings
//! file can be shared or committed without leaking it.
//!
//! The file is read once at startup. The app rewrites it when you change the
//! font size or font family (in the settings panel, via shortcuts, or from the
//! Ctrl-K palette), so hand edits to other keys survive only if they are valid
//! ones we know about.
//!
//! The colour theme is **not** here: it is chosen in Settings and stored in
//! `state.toml` (`state.rs`). An old `theme = "dark"` / `"light"` line in this
//! file is still read once, to carry that choice over, and is dropped the next
//! time the file is saved.

use crate::ai::AiProvider;
use crate::storage::write_atomic;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const MIN_FONT_SIZE: f32 = 10.0;
pub const MAX_FONT_SIZE: f32 = 32.0;
pub const DEFAULT_FONT_SIZE: f32 = 16.0;

/// The built-in colour themes. The chosen one is stored in `state.toml`
/// (`state.rs`), by the names below.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeKind {
    #[default]
    TokyoNight,
    // Before themes were selectable there was one dark palette (Catppuccin
    // Mocha) and `theme = "dark"` in `config.toml`; keep reading that.
    #[serde(alias = "dark")]
    CatppuccinMocha,
    Light,
}

impl ThemeKind {
    /// Every theme, in the order the settings picker lists them.
    pub const ALL: [ThemeKind; 3] = [
        ThemeKind::TokyoNight,
        ThemeKind::CatppuccinMocha,
        ThemeKind::Light,
    ];

    /// The name shown in Settings.
    pub fn label(self) -> &'static str {
        match self {
            ThemeKind::TokyoNight => "Tokyo Night",
            ThemeKind::CatppuccinMocha => "Catppuccin Mocha",
            ThemeKind::Light => "Light",
        }
    }

    /// What the "Toggle dark/light theme" command and Ctrl+Shift+T do: from
    /// any dark theme to Light, and from Light back to the default dark one.
    pub fn toggled(self) -> Self {
        match self {
            ThemeKind::Light => ThemeKind::TokyoNight,
            ThemeKind::TokyoNight | ThemeKind::CatppuccinMocha => ThemeKind::Light,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
// Any key missing from the file falls back to its default.
#[serde(default)]
pub struct Config {
    /// **Legacy, read-only.** The theme used to live here as `theme = "dark"`
    /// or `"light"`. It is now chosen in Settings and stored in `state.toml`;
    /// this is read once at startup so an existing choice carries over (see
    /// `NoteSec::new`), and is never written back.
    #[serde(rename = "theme", skip_serializing)]
    pub legacy_theme: Option<ThemeKind>,
    pub font_size: f32,
    /// `None` means "use the system UI font".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    /// Git auto-backup (`backup.rs`): commit the graph folder to a local
    /// git repository after changes. Off unless the user turns it on.
    pub git_backup: bool,
    /// Which AI backend is active (decision 42): Local (default), API key
    /// or Off. Never changes on its own.
    pub ai_provider: AiProvider,
    /// Local mode: the server's OpenAI-compatible API (`ai.rs`); only
    /// loopback addresses are accepted.
    pub ai_endpoint: String,
    /// Local mode: the chat model's id; empty means the server's first
    /// chat model.
    pub ai_model: String,
    /// Local mode: the embedding model's id (semantic search, decision
    /// 43); empty means use the chat model.
    pub ai_embedding_model: String,
    /// API key mode: the provider's https base URL.
    pub ai_api_base: String,
    /// API key mode: the model id.
    pub ai_api_model: String,
    /// API key mode: the embedding model's id; empty means `ai_api_model`.
    /// Separate from Local's: model names differ between servers.
    pub ai_api_embedding_model: String,
    /// Vim keybindings in the block editor (`vim.rs`, decision 48). Off
    /// unless the user turns it on.
    pub vim_mode: bool,
    /// The web clipper (`clipper/`, decision 51): an HTTP endpoint on
    /// 127.0.0.1. Off unless the user turns it on.
    pub web_clipper: bool,
    /// Its port. Out of 1024-65535 reads as the default.
    pub clipper_port: u16,
    /// A recorder command (program, then arguments; `{file}` for the WAV
    /// to write) instead of the detected one. Empty: detect.
    pub voice_recorder: Vec<String>,
    /// whisper.cpp's program, for local transcription. Empty: none.
    pub whisper_binary: String,
    /// Its model file.
    pub whisper_model: String,
    /// `auto` or a language code (`en`, `de`…).
    pub whisper_language: String,
    /// Transcribe each new voice note when it's recorded.
    pub voice_auto_transcribe: bool,
    /// Enabled plugins: id -> hash of the `plugin.wasm` that was enabled
    /// (decision 55). Plugins not listed, or whose binary changed, are off.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub plugins: std::collections::BTreeMap<String, String>,
}

/// OpenAI's API, the most common OpenAI-compatible base URL.
pub const DEFAULT_API_BASE: &str = "https://api.openai.com/v1";

impl Default for Config {
    fn default() -> Self {
        Config {
            legacy_theme: None,
            font_size: DEFAULT_FONT_SIZE,
            font_family: None,
            git_backup: false,
            ai_provider: AiProvider::default(),
            ai_endpoint: crate::ai::DEFAULT_ENDPOINT.to_string(),
            ai_model: String::new(),
            ai_embedding_model: String::new(),
            ai_api_base: DEFAULT_API_BASE.to_string(),
            ai_api_model: String::new(),
            ai_api_embedding_model: String::new(),
            vim_mode: false,
            web_clipper: false,
            clipper_port: crate::clipper::DEFAULT_PORT,
            voice_recorder: Vec::new(),
            whisper_binary: String::new(),
            whisper_model: String::new(),
            whisper_language: "auto".into(),
            voice_auto_transcribe: false,
            plugins: Default::default(),
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
        if self.clipper_port < 1024 {
            self.clipper_port = crate::clipper::DEFAULT_PORT;
        }
        self.whisper_language = crate::voice::whisper::language(&self.whisper_language);
        // An empty family name means "unset".
        if self
            .font_family
            .as_deref()
            .is_some_and(|f| f.trim().is_empty())
        {
            self.font_family = None;
        }
        for field in [
            &mut self.ai_endpoint,
            &mut self.ai_model,
            &mut self.ai_embedding_model,
            &mut self.ai_api_base,
            &mut self.ai_api_model,
            &mut self.ai_api_embedding_model,
        ] {
            *field = field.trim().to_string();
        }
        if self.ai_endpoint.is_empty() {
            self.ai_endpoint = crate::ai::DEFAULT_ENDPOINT.to_string();
        }
        if self.ai_api_base.is_empty() {
            self.ai_api_base = DEFAULT_API_BASE.to_string();
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
    fn vim_mode_is_off_unless_the_file_says_so() {
        assert!(!Config::default().vim_mode);
        let dir = temp_dir("vim");
        fs::write(Config::path(&dir), "theme = \"light\"\n").unwrap();
        assert!(!Config::load(&dir).vim_mode);
        fs::write(Config::path(&dir), "vim_mode = true\n").unwrap();
        assert!(Config::load(&dir).vim_mode);
        let _ = fs::remove_dir_all(dir);
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
            // Never written, so it must not survive a round trip.
            legacy_theme: None,
            font_size: 20.0,
            font_family: Some("Inter".into()),
            git_backup: true,
            ai_provider: AiProvider::Api,
            ai_endpoint: "http://127.0.0.1:8080/v1".into(),
            ai_model: "qwen2.5-7b".into(),
            ai_embedding_model: "nomic-embed-text".into(),
            ai_api_base: "https://api.x.ai/v1".into(),
            ai_api_model: "grok-4".into(),
            ai_api_embedding_model: "text-embedding-3-small".into(),
            vim_mode: true,
            web_clipper: true,
            clipper_port: 31337,
            voice_recorder: vec!["rec".into(), "{file}".into()],
            whisper_binary: "/usr/bin/whisper-cli".into(),
            whisper_model: "/m/ggml-base.bin".into(),
            whisper_language: "de".into(),
            voice_auto_transcribe: true,
            plugins: [("word-count".to_string(), "ab12".to_string())].into(),
        };
        config.save(&dir).unwrap();
        assert_eq!(Config::load(&dir), config);
        // No temp file left behind by the atomic write.
        assert!(!dir.join(".config.toml.tmp").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_privileged_or_zero_clipper_port_reads_as_the_default() {
        let dir = temp_dir("clipper-port");
        for bad in ["clipper_port = 0", "clipper_port = 80"] {
            fs::write(dir.join("config.toml"), bad).unwrap();
            let config = Config::load(&dir);
            assert_eq!(config.clipper_port, crate::clipper::DEFAULT_PORT);
            assert!(!config.web_clipper);
        }
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
    fn the_old_theme_key_is_read_but_never_written() {
        let dir = temp_dir("legacy-theme");
        // "dark" was the one dark palette, now called Catppuccin Mocha.
        fs::write(Config::path(&dir), "theme = \"dark\"\nfont_size = 18.0\n").unwrap();
        let config = Config::load(&dir);
        assert_eq!(config.legacy_theme, Some(ThemeKind::CatppuccinMocha));
        assert_eq!(config.font_size, 18.0);
        // Saving for any other reason drops the key (the app has copied it to
        // state.toml by then).
        config.save(&dir).unwrap();
        let text = fs::read_to_string(Config::path(&dir)).unwrap();
        assert!(!text.contains("theme"), "unexpected theme key in:\n{text}");
        assert_eq!(Config::load(&dir).legacy_theme, None);
        // The new names work in the old place too.
        fs::write(Config::path(&dir), "theme = \"tokyo-night\"\n").unwrap();
        assert_eq!(Config::load(&dir).legacy_theme, Some(ThemeKind::TokyoNight));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn partial_files_fill_in_defaults() {
        let dir = temp_dir("partial");
        fs::write(Config::path(&dir), "theme = \"light\"\n").unwrap();
        let c = Config::load(&dir);
        assert_eq!(c.legacy_theme, Some(ThemeKind::Light));
        assert_eq!(c.font_size, DEFAULT_FONT_SIZE);
        assert!(!c.git_backup, "backup is off unless turned on");
        assert_eq!(c.ai_endpoint, crate::ai::DEFAULT_ENDPOINT);
        assert_eq!(c.ai_model, "");
        assert_eq!(
            c.ai_provider,
            AiProvider::Local,
            "local is the default mode"
        );
        assert_eq!(c.ai_api_base, DEFAULT_API_BASE);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ai_provider_reads_each_mode_and_never_holds_the_key() {
        let dir = temp_dir("provider");
        for (text, mode) in [
            ("local", AiProvider::Local),
            ("api", AiProvider::Api),
            ("off", AiProvider::Off),
        ] {
            fs::write(Config::path(&dir), format!("ai_provider = \"{text}\"\n")).unwrap();
            assert_eq!(Config::load(&dir).ai_provider, mode);
        }
        let config = Config {
            ai_provider: AiProvider::Off,
            ..Config::default()
        };
        config.save(&dir).unwrap();
        let text = fs::read_to_string(Config::path(&dir)).unwrap();
        assert!(text.contains("ai_provider = \"off\""));
        assert!(!text.contains("key"), "the API key lives in state.toml");
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
        fs::write(
            Config::path(&dir),
            "ai_endpoint = \" \"\nai_model = \" m \"\n",
        )
        .unwrap();
        let c = Config::load(&dir);
        assert_eq!(c.ai_endpoint, crate::ai::DEFAULT_ENDPOINT);
        assert_eq!(c.ai_model, "m");
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
