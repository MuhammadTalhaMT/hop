//! Loading and validating `hop`'s TOML configuration file.
//!
//! The schema is defined in the spec's Configuration section
//! (`docs/superpowers/specs/2026-08-31-hop-design.md`): `role`, `bind`,
//! `[[peers]]` with `id` and `[peers.remap]`, `[layout]`, `[security]`
//! with `key_file`, and `[input]` with `panic_hotkey`.
//!
//! A config file is user input. Every failure to parse or validate it
//! returns a `ConfigError` that names the field or key at fault rather
//! than panicking or silently ignoring the problem. This matters most
//! for `[peers.remap]` and `[layout]`: a remap line or edge name that is
//! silently dropped leaves the user with a key or edge that mysteriously
//! does not work and no way to tell why.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use hop_core::RemapTable;
use serde::Deserialize;

use crate::keymap;

/// Whether this machine listens for peers to connect (the Mac, in the
/// spec's usual layout) or dials out to one (the PC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Server,
    Client,
}

/// One entry under `[[peers]]`: the id a client presents during the
/// handshake, and the remap table applied to keys coming from it.
#[derive(Debug, Clone)]
pub struct PeerConfig {
    pub id: String,
    pub remap: RemapTable,
}

/// `[layout]`: which peer id sits at each screen edge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layout {
    pub top: Option<String>,
    pub bottom: Option<String>,
    pub left: Option<String>,
    pub right: Option<String>,
}

/// `[discovery]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveryConfig {
    pub enabled: bool,
}

/// `[security]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityConfig {
    pub key_file: PathBuf,
}

/// `[input]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputConfig {
    pub panic_hotkey: Option<String>,
}

/// A fully validated `hop` configuration, covering both the server and
/// client roles described in the spec.
#[derive(Debug, Clone)]
pub struct Config {
    pub role: Role,
    /// Server only: the address to listen on.
    pub bind: Option<String>,
    /// Client only: the id this machine presents during the handshake.
    pub id: Option<String>,
    /// Client only: the server to dial, either a discovered name or
    /// `host:port`.
    pub server: Option<String>,
    /// Server only: the peers allowed to connect, matched by id.
    pub peers: Vec<PeerConfig>,
    pub layout: Layout,
    pub discovery: DiscoveryConfig,
    pub security: SecurityConfig,
    pub input: InputConfig,
}

/// Every way loading or validating a config file can fail. Each variant
/// names the field, key, or file at fault so a user can fix their config
/// without guessing.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read config file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not parse {path} as TOML: {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("config is missing required field '{field}'")]
    MissingField { field: String },

    #[error("config field '{field}' has invalid value '{value}'")]
    InvalidValue { field: String, value: String },

    #[error(
        "peer '{peer}': [peers.remap] has unknown key name '{key}'; expected a name like \"LeftGui\", \"A\", \"F1\", or \"Up\""
    )]
    UnknownRemapKey { peer: String, key: String },

    #[error(
        "peer '{peer}': [peers.remap] maps '{key}' to unknown key name '{value}'; expected a name like \"LeftGui\", \"A\", \"F1\", or \"Up\""
    )]
    UnknownRemapValue {
        peer: String,
        key: String,
        value: String,
    },

    #[error(
        "[layout] has unknown edge name '{edge}'; expected one of \"top\", \"bottom\", \"left\", \"right\""
    )]
    UnknownEdge { edge: String },
}

/// The raw shape of the TOML file, before validation. Every field is
/// optional here so a missing required field can be reported by name
/// rather than as an opaque deserialization failure.
#[derive(Debug, Deserialize)]
struct RawConfig {
    role: Option<String>,
    bind: Option<String>,
    id: Option<String>,
    server: Option<String>,
    #[serde(default)]
    peers: Vec<RawPeer>,
    #[serde(default)]
    layout: HashMap<String, String>,
    discovery: Option<RawDiscovery>,
    security: Option<RawSecurity>,
    input: Option<RawInput>,
}

#[derive(Debug, Deserialize)]
struct RawPeer {
    id: Option<String>,
    #[serde(default)]
    remap: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct RawDiscovery {
    enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct RawSecurity {
    key_file: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawInput {
    panic_hotkey: Option<String>,
}

impl Config {
    /// Read and validate the config file at `path`.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text, path)
    }

    fn parse(text: &str, path: &Path) -> Result<Config, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|source| ConfigError::Toml {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: RawConfig) -> Result<Config, ConfigError> {
        let role = match raw.role.as_deref() {
            None => {
                return Err(ConfigError::MissingField {
                    field: "role".into(),
                })
            }
            Some("server") => Role::Server,
            Some("client") => Role::Client,
            Some(other) => {
                return Err(ConfigError::InvalidValue {
                    field: "role".into(),
                    value: other.to_string(),
                })
            }
        };

        let peers = raw
            .peers
            .into_iter()
            .enumerate()
            .map(|(index, raw_peer)| Self::peer_from_raw(index, raw_peer))
            .collect::<Result<Vec<_>, ConfigError>>()?;

        let mut layout = Layout::default();
        for (edge, peer_id) in raw.layout {
            match edge.as_str() {
                "top" => layout.top = Some(peer_id),
                "bottom" => layout.bottom = Some(peer_id),
                "left" => layout.left = Some(peer_id),
                "right" => layout.right = Some(peer_id),
                _ => return Err(ConfigError::UnknownEdge { edge }),
            }
        }

        let discovery = DiscoveryConfig {
            enabled: raw.discovery.and_then(|d| d.enabled).unwrap_or(false),
        };

        let key_file =
            raw.security
                .and_then(|s| s.key_file)
                .ok_or_else(|| ConfigError::MissingField {
                    field: "security.key_file".into(),
                })?;
        let security = SecurityConfig {
            key_file: PathBuf::from(key_file),
        };

        let input = InputConfig {
            panic_hotkey: raw.input.and_then(|i| i.panic_hotkey),
        };

        Ok(Config {
            role,
            bind: raw.bind,
            id: raw.id,
            server: raw.server,
            peers,
            layout,
            discovery,
            security,
            input,
        })
    }

    fn peer_from_raw(index: usize, raw_peer: RawPeer) -> Result<PeerConfig, ConfigError> {
        let id = raw_peer.id.ok_or_else(|| ConfigError::MissingField {
            field: format!("peers[{index}].id"),
        })?;

        let mut remap = RemapTable::new();
        for (key, value) in raw_peer.remap {
            let from = keymap::lookup(&key).ok_or_else(|| ConfigError::UnknownRemapKey {
                peer: id.clone(),
                key: key.clone(),
            })?;
            let to = keymap::lookup(&value).ok_or_else(|| ConfigError::UnknownRemapValue {
                peer: id.clone(),
                key: key.clone(),
                value: value.clone(),
            })?;
            remap.insert(from, to);
        }

        Ok(PeerConfig { id, remap })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Writes `contents` to a uniquely named file in the OS temp
    /// directory and returns its path. Avoids pulling in a temp-file
    /// crate for what is otherwise a couple of lines.
    fn write_config(contents: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hop-config-test-{}-{n}-{nanos}.toml",
            std::process::id()
        ));
        fs::write(&path, contents).expect("write temp config file");
        path
    }

    const SERVER_CONFIG: &str = r#"
role = "server"
bind = "0.0.0.0:24810"

[[peers]]
id = "pc"

[peers.remap]
LeftGui = "LeftCtrl"
RightGui = "RightCtrl"
LeftAlt = "LeftAlt"

[layout]
top = "pc"

[security]
key_file = "~/.config/hop/key"

[input]
panic_hotkey = "LeftCtrl+LeftAlt+Escape"
"#;

    const CLIENT_CONFIG: &str = r#"
role = "client"
id = "pc"
server = "talhas-mac"

[discovery]
enabled = true

[security]
key_file = "%APPDATA%\\hop\\key"
"#;

    #[test]
    fn valid_server_config_parses() {
        let path = write_config(SERVER_CONFIG);
        let config = Config::load(&path).expect("valid server config should load");
        fs::remove_file(&path).ok();

        assert_eq!(config.role, Role::Server);
        assert_eq!(config.bind.as_deref(), Some("0.0.0.0:24810"));
        assert_eq!(config.peers.len(), 1);
        assert_eq!(config.peers[0].id, "pc");
        assert_eq!(
            config.peers[0].remap.apply(hop_proto::Usage::LEFT_GUI),
            hop_proto::Usage::LEFT_CTRL
        );
        assert_eq!(
            config.peers[0].remap.apply(hop_proto::Usage::RIGHT_GUI),
            hop_proto::Usage::RIGHT_CTRL
        );
        assert_eq!(
            config.peers[0].remap.apply(hop_proto::Usage::LEFT_ALT),
            hop_proto::Usage::LEFT_ALT
        );
        assert_eq!(config.layout.top.as_deref(), Some("pc"));
        assert_eq!(config.layout.bottom, None);
        assert_eq!(config.security.key_file, PathBuf::from("~/.config/hop/key"));
        assert_eq!(
            config.input.panic_hotkey.as_deref(),
            Some("LeftCtrl+LeftAlt+Escape")
        );
    }

    #[test]
    fn valid_client_config_parses() {
        let path = write_config(CLIENT_CONFIG);
        let config = Config::load(&path).expect("valid client config should load");
        fs::remove_file(&path).ok();

        assert_eq!(config.role, Role::Client);
        assert_eq!(config.id.as_deref(), Some("pc"));
        assert_eq!(config.server.as_deref(), Some("talhas-mac"));
        assert!(config.discovery.enabled);
        assert_eq!(
            config.security.key_file,
            PathBuf::from("%APPDATA%\\hop\\key")
        );
        assert!(config.peers.is_empty());
    }

    #[test]
    fn missing_key_file_is_a_named_error() {
        let path = write_config(
            r#"
role = "server"
bind = "0.0.0.0:24810"
"#,
        );
        let err = Config::load(&path).expect_err("missing key_file should be rejected");
        fs::remove_file(&path).ok();

        let message = err.to_string();
        assert!(
            message.contains("security.key_file"),
            "error should name the missing field, got: {message}"
        );
    }

    #[test]
    fn unknown_remap_key_is_a_named_error() {
        let path = write_config(
            r#"
role = "server"
bind = "0.0.0.0:24810"

[[peers]]
id = "pc"

[peers.remap]
NotAKey = "LeftCtrl"

[security]
key_file = "~/.config/hop/key"
"#,
        );
        let err = Config::load(&path).expect_err("unknown remap key should be rejected");
        fs::remove_file(&path).ok();

        let message = err.to_string();
        assert!(
            message.contains("NotAKey"),
            "error should name the offending key, got: {message}"
        );
        assert!(matches!(err, ConfigError::UnknownRemapKey { .. }));
    }

    #[test]
    fn unknown_remap_value_is_a_named_error() {
        let path = write_config(
            r#"
role = "server"
bind = "0.0.0.0:24810"

[[peers]]
id = "pc"

[peers.remap]
LeftGui = "NotAKey"

[security]
key_file = "~/.config/hop/key"
"#,
        );
        let err = Config::load(&path).expect_err("unknown remap value should be rejected");
        fs::remove_file(&path).ok();

        let message = err.to_string();
        assert!(
            message.contains("NotAKey"),
            "error should name the offending value, got: {message}"
        );
        assert!(matches!(err, ConfigError::UnknownRemapValue { .. }));
    }

    #[test]
    fn unknown_layout_edge_is_a_named_error() {
        let path = write_config(
            r#"
role = "server"
bind = "0.0.0.0:24810"

[layout]
diagonal = "pc"

[security]
key_file = "~/.config/hop/key"
"#,
        );
        let err = Config::load(&path).expect_err("unknown layout edge should be rejected");
        fs::remove_file(&path).ok();

        let message = err.to_string();
        assert!(
            message.contains("diagonal"),
            "error should name the offending edge, got: {message}"
        );
        assert!(matches!(err, ConfigError::UnknownEdge { .. }));
    }

    #[test]
    fn missing_role_is_an_error() {
        let path = write_config(
            r#"
bind = "0.0.0.0:24810"

[security]
key_file = "~/.config/hop/key"
"#,
        );
        let err = Config::load(&path).expect_err("missing role should be rejected");
        fs::remove_file(&path).ok();

        let message = err.to_string();
        assert!(
            message.contains("role"),
            "error should name the missing field, got: {message}"
        );
        assert!(matches!(err, ConfigError::MissingField { .. }));
    }

    #[test]
    fn invalid_role_value_is_an_error() {
        let path = write_config(
            r#"
role = "supervisor"

[security]
key_file = "~/.config/hop/key"
"#,
        );
        let err = Config::load(&path).expect_err("invalid role should be rejected");
        fs::remove_file(&path).ok();

        let message = err.to_string();
        assert!(
            message.contains("supervisor"),
            "error should name the invalid value, got: {message}"
        );
    }

    #[test]
    fn missing_config_file_is_an_io_error() {
        let path = std::env::temp_dir().join("hop-config-test-does-not-exist.toml");
        let err = Config::load(&path).expect_err("missing file should be rejected");
        assert!(matches!(err, ConfigError::Io { .. }));
    }
}
