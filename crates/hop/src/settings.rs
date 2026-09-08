//! The handful of client settings hop's window can change, and writing
//! them back to `config.toml`.
//!
//! Deliberately narrow. `Config` is the full, validated shape of a config
//! file for both roles; this is only the client fields a person would
//! ever reach for, and only the client's, because the window runs on the
//! PC. Everything else in the file is either structural (`role`, `id`) or
//! something you set once (`key_file`).
//!
//! Writing REGENERATES the file rather than editing it in place. hop's
//! config is small and every field is known here, so a generated file can
//! carry better comments than most people would write. The cost is
//! honest and worth stating: comments a user added by hand do not
//! survive a save from the window. Editing TOML in place while preserving
//! formatting means a format-preserving parser and a lot of care, for a
//! file with six settings in it.

use std::path::{Path, PathBuf};

use crate::config::Config;

/// What the window can change.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// The server's `host:port`.
    pub server: String,
    /// This machine's id, as the server's `[[peers]]` list knows it.
    pub id: String,
    /// Where the shared key lives, as written in the file, unexpanded.
    pub key_file: String,
    /// Which edge of this machine's desktop faces the Mac.
    pub return_edge: String,
    pub mouse_scale: f64,
    /// `None` for "centred under the primary monitor".
    pub anchor: Option<f32>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server: String::new(),
            id: "pc".into(),
            key_file: "%APPDATA%\\hop\\key".into(),
            return_edge: "bottom".into(),
            mouse_scale: 1.0,
            anchor: None,
        }
    }
}

impl Settings {
    /// Pulls the editable fields out of a loaded config.
    pub fn from_config(config: &Config) -> Self {
        Self {
            server: config.server.clone().unwrap_or_default(),
            id: config.id.clone().unwrap_or_else(|| "pc".into()),
            key_file: config.security.key_file.display().to_string(),
            return_edge: config
                .input
                .return_edge
                .clone()
                .unwrap_or_else(|| "bottom".into()),
            mouse_scale: config.input.mouse_scale,
            anchor: config.input.anchor,
        }
    }

    /// Reads the settings out of the config file at `path`, or the
    /// defaults if there is no file there yet.
    ///
    /// A missing file is not an error: it is what the first run looks
    /// like, and the window's job in that case is to offer somewhere to
    /// type the address rather than an error about a file the user has
    /// never heard of.
    pub fn load(path: &Path) -> Self {
        match Config::load(path) {
            Ok(config) => Self::from_config(&config),
            Err(_) => Self::default(),
        }
    }

    /// The complete `config.toml` these settings describe.
    pub fn to_toml(&self) -> String {
        let mut out = String::new();
        out.push_str("# Written by hop. Editing by hand is fine; hop\n");
        out.push_str("# rewrites this whole file when you save from its window,\n");
        out.push_str("# so comments you add here will not survive that.\n\n");
        out.push_str("role = \"client\"\n");
        out.push_str(&format!("id = {}\n", quote(&self.id)));
        out.push_str("# The Mac's address and port.\n");
        out.push_str(&format!("server = {}\n\n", quote(&self.server)));

        out.push_str("[security]\n");
        out.push_str("# Both machines must hold the SAME key file.\n");
        out.push_str(&format!("key_file = {}\n\n", quote(&self.key_file)));

        out.push_str("[input]\n");
        out.push_str("# The edge of this desktop that faces the Mac. Focus both\n");
        out.push_str("# arrives on and leaves through this edge, and it must be\n");
        out.push_str("# the mirror of the Mac's [layout] edge.\n");
        out.push_str(&format!("return_edge = {}\n", quote(&self.return_edge)));
        out.push_str("# Lower this if the pointer travels further here than on\n");
        out.push_str("# the Mac.\n");
        out.push_str(&format!(
            "mouse_scale = {}\n",
            format_float(self.mouse_scale)
        ));
        match self.anchor {
            Some(anchor) => {
                out.push_str("# Where along return_edge the Mac sits, 0.0 to 1.0.\n");
                out.push_str(&format!("anchor = {}\n", format_float(f64::from(anchor))));
            }
            None => {
                out.push_str("# Where along return_edge the Mac sits, 0.0 to 1.0.\n");
                out.push_str("# Unset means centred under the primary monitor.\n");
                out.push_str("# anchor = 0.5\n");
            }
        }
        out
    }

    /// Writes the settings to `path`, creating the directory if needed.
    ///
    /// Written to a neighbouring temporary file and renamed, so a crash
    /// or a full disk part way through leaves the previous config intact
    /// rather than a half-written file hop will refuse to start from.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        // Refuse to write something hop itself could not load. Saving a
        // config that bricks the next start, from the window whose whole
        // purpose is to stop people hand-editing TOML, would be the worst
        // possible failure here.
        self.validate()?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
        }
        let staged = path.with_extension("toml.new");
        std::fs::write(&staged, self.to_toml())
            .map_err(|e| format!("could not write {}: {e}", staged.display()))?;
        std::fs::rename(&staged, path)
            .map_err(|e| format!("could not replace {}: {e}", path.display()))?;
        Ok(())
    }

    /// The checks that would otherwise only fire at load time, so the
    /// window can say what is wrong while the user is still looking at
    /// the field.
    pub fn validate(&self) -> Result<(), String> {
        if self.server.trim().is_empty() {
            return Err("the server address is empty".into());
        }
        if !self.server.contains(':') {
            return Err("the server address needs a port, like 192.168.1.42:24810".into());
        }
        if self.id.trim().is_empty() {
            return Err("this machine needs an id, matching the Mac's [[peers]] list".into());
        }
        if self.key_file.trim().is_empty() {
            return Err("the key file path is empty".into());
        }
        if !matches!(
            self.return_edge.as_str(),
            "top" | "bottom" | "left" | "right"
        ) {
            return Err(format!("{} is not an edge name", self.return_edge));
        }
        if !(self.mouse_scale.is_finite() && self.mouse_scale > 0.0) {
            return Err("pointer speed must be greater than zero".into());
        }
        if let Some(anchor) = self.anchor {
            if !(anchor.is_finite() && (0.0..=1.0).contains(&anchor)) {
                return Err("the anchor must be between 0.0 and 1.0".into());
            }
        }
        Ok(())
    }
}

/// Where hop keeps its config when nobody says otherwise.
pub fn default_config_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join("hop").join("config.toml")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let base = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base)
            .join(".config")
            .join("hop")
            .join("config.toml")
    }
}

/// A TOML basic string. Backslashes matter here: Windows paths are full
/// of them, and `%APPDATA%\hop\key` written raw is a different path after
/// TOML unescapes it.
fn quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// TOML needs a decimal point to read a number as a float, and
/// `mouse_scale` and `anchor` are both floats in the schema. Printing
/// `1` where `1.0` is meant makes the file fail to load.
fn format_float(value: f64) -> String {
    if value == value.trunc() && value.is_finite() {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Settings {
        Settings {
            server: "192.168.1.42:24810".into(),
            id: "pc".into(),
            key_file: "%APPDATA%\\hop\\key".into(),
            return_edge: "bottom".into(),
            mouse_scale: 0.6,
            anchor: Some(0.25),
        }
    }

    /// Writes `toml` to a temporary file and loads it back as a `Config`.
    fn round_trip(settings: &Settings) -> Settings {
        let dir = std::env::temp_dir().join(format!(
            "hop-settings-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.toml");
        settings.save(&path).expect("save");
        let loaded = Settings::load(&path);
        std::fs::remove_dir_all(&dir).ok();
        loaded
    }

    // The property that matters: anything the window saves, hop can load,
    // and it means the same thing. Without this the window is a way to
    // produce a config file that stops hop starting.
    #[test]
    fn everything_the_window_writes_loads_back_unchanged() {
        let original = sample();
        assert_eq!(round_trip(&original), original);
    }

    #[test]
    fn an_unset_anchor_survives_as_unset_rather_than_becoming_zero() {
        // 0.0 is a real anchor meaning the far left, so writing it for
        // "not set" would quietly move every crossing to the left edge.
        let mut settings = sample();
        settings.anchor = None;
        assert_eq!(round_trip(&settings).anchor, None);
    }

    #[test]
    fn a_windows_path_survives_tomls_own_escaping() {
        // A backslash is TOML's escape character, so a path written raw
        // comes back as something else entirely, or fails to parse.
        let mut settings = sample();
        settings.key_file = "C:\\Users\\someone\\hop\\key".into();
        assert_eq!(
            round_trip(&settings).key_file,
            "C:\\Users\\someone\\hop\\key"
        );
    }

    #[test]
    fn a_whole_number_speed_is_written_as_a_float() {
        // `mouse_scale = 1` is an integer in TOML and the schema wants a
        // float, so the file would fail to load.
        let mut settings = sample();
        settings.mouse_scale = 1.0;
        assert!(settings.to_toml().contains("mouse_scale = 1.0"));
        assert_eq!(round_trip(&settings).mouse_scale, 1.0);
    }

    #[test]
    fn a_missing_config_file_reads_as_defaults_not_an_error() {
        // The first run. The window's job is to offer somewhere to type
        // the address, not to complain about a file nobody has made yet.
        let settings = Settings::load(Path::new("/nonexistent/hop/config.toml"));
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn settings_hop_could_not_load_are_refused_before_they_are_written() {
        let bad = [
            (
                Settings {
                    server: String::new(),
                    ..sample()
                },
                "empty address",
            ),
            (
                Settings {
                    server: "192.168.1.42".into(),
                    ..sample()
                },
                "no port",
            ),
            (
                Settings {
                    return_edge: "diagonal".into(),
                    ..sample()
                },
                "bad edge",
            ),
            (
                Settings {
                    mouse_scale: 0.0,
                    ..sample()
                },
                "zero speed",
            ),
            (
                Settings {
                    mouse_scale: -1.0,
                    ..sample()
                },
                "negative speed",
            ),
            (
                Settings {
                    anchor: Some(1.5),
                    ..sample()
                },
                "anchor past the end",
            ),
            (
                Settings {
                    id: "  ".into(),
                    ..sample()
                },
                "blank id",
            ),
        ];
        for (settings, what) in bad {
            assert!(settings.validate().is_err(), "{what} should be refused");
            let path = std::env::temp_dir().join("hop-never-written.toml");
            assert!(
                settings.save(&path).is_err(),
                "{what} should not be written"
            );
            assert!(!path.exists(), "{what} left a file behind");
        }
    }
}
