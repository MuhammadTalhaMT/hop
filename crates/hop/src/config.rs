//! Loading and validating `hop`'s TOML configuration file.
//!
//! The schema is defined in the spec's Configuration section
//! (`docs/superpowers/specs/2026-08-31-hop-design.md`): `role`, `bind`,
//! `[[peers]]` with `id` and `[peers.remap]`, `[layout]`, `[security]`
//! with `key_file`, and `[input]` with `panic_hotkey` and `return_edge`.
//!
//! # Example: server (the Mac)
//!
//! ```toml
//! role = "server"
//! bind = "0.0.0.0:24810"
//!
//! [[peers]]
//! id = "pc"                  # matched against the id the client presents
//!
//! [peers.remap]
//! LeftGui = "LeftCtrl"
//! RightGui = "RightCtrl"
//! LeftAlt = "LeftAlt"
//!
//! [layout]
//! top = "pc"                 # the edge input crosses to reach "pc"
//!
//! [security]
//! key_file = "~/.config/hop/key"   # a leading '~' expands to $HOME
//!
//! [input]
//! # Required for role = "server": the emergency escape that returns
//! # control to this machine if input ever gets stuck on the peer (see
//! # ConfigError::ServerMissingPanicHotkey below). Without it there is
//! # no way to recover except killing hop from another machine.
//! panic_hotkey = "LeftCtrl+LeftAlt+Escape"
//! ```
//!
//! # Example: client (the PC)
//!
//! ```toml
//! role = "client"
//! id = "pc"
//! server = "192.168.18.90:24810"   # host:port; discovery by name is not
//!                                   # implemented yet, so a bare name
//!                                   # like "talhas-mac" is rejected
//!
//! [security]
//! key_file = "%APPDATA%\\hop\\key"   # '%VAR%' expands on Windows
//!
//! [input]
//! # Required for role = "client": the edge of THIS machine's screen
//! # that hands focus back to the server. It must be the mirror image
//! # of the server's own [layout] edge above: the server's `top = "pc"`
//! # pairs with the client's `return_edge = "bottom"`, `left` pairs
//! # with `right`. The two settings live in separate config files on
//! # separate machines, so this pairing cannot be checked at load time;
//! # get it backwards and focus crosses to the PC but has no automatic
//! # way back, only the server's panic hotkey.
//! return_edge = "bottom"
//! ```
//!
//! A config file is user input. Every failure to parse or validate it
//! returns a `ConfigError` that names the field or key at fault rather
//! than panicking or silently ignoring the problem. This matters most
//! for `[peers.remap]` and `[layout]`: a remap line or edge name that is
//! silently dropped leaves the user with a key or edge that mysteriously
//! does not work and no way to tell why. The same is true of the two
//! escape hatches above: a missing `panic_hotkey` on the server or a
//! missing `return_edge` on the client fails config loading loudly
//! instead of leaving the user with no way back to the local machine.

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
    /// Required for `role = "server"` (see
    /// [`ConfigError::ServerMissingPanicHotkey`]): the emergency escape
    /// that returns control to this machine if input ever gets stuck on
    /// the peer. Not required, and never consulted, for
    /// `role = "client"`: only the server ever captures input.
    pub panic_hotkey: Option<String>,
    /// Client only: which edge of this machine's virtual screen hands
    /// focus back to the server, the mirror image of the server's own
    /// `[layout]` edge. Required for `role = "client"`: it is the only
    /// automatic way focus ever returns (see CRITICAL 2 in the
    /// whole-branch review that added it), short of the panic hotkey,
    /// which lives only on the server.
    ///
    /// Must be the opposite edge from the server's `[layout]` entry for
    /// this peer: a server `top = "pc"` pairs with a client
    /// `return_edge = "bottom"`, and `left` pairs with `right`. The two
    /// settings live in separate config files on separate machines, so
    /// this pairing cannot be checked at load time; get it backwards and
    /// focus crosses over but has no automatic way back.
    pub return_edge: Option<String>,
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

    #[error(
        "role = \"server\" requires 'bind'; add bind = \"<host>:<port>\" (for example \"0.0.0.0:24810\") to specify the address to listen on"
    )]
    ServerMissingBind,

    #[error(
        "role = \"server\" requires at least one [[peers]] entry; a server with no peers can never accept a connection. Add a [[peers]] section with an id"
    )]
    ServerMissingPeers,

    #[error(
        "role = \"client\" requires 'id'; add id = \"<name>\" to identify this machine during the handshake"
    )]
    ClientMissingId,

    #[error(
        "role = \"client\" requires 'server'; add server = \"<host:port or discovered name>\" naming the server to connect to"
    )]
    ClientMissingServer,

    #[error(
        "[layout] edge '{edge}' names peer '{peer}', which is not defined in [[peers]]; add a [[peers]] entry with id = \"{peer}\" or fix the typo"
    )]
    LayoutUnknownPeer { edge: String, peer: String },

    #[error(
        "role = \"client\" requires '[input] return_edge'; add return_edge = \"top\", \"bottom\", \"left\", or \"right\" naming the edge of this machine's screen that hands focus back to the server (the mirror image of the server's own [layout] edge). Without it the only way focus ever returns is the panic hotkey, which lives on the server, not here"
    )]
    ClientMissingReturnEdge,

    #[error(
        "[input] return_edge has unknown value '{value}'; expected one of \"top\", \"bottom\", \"left\", \"right\""
    )]
    UnknownReturnEdge { value: String },

    #[error(
        "a server config must set [input] panic_hotkey. It is the emergency escape that returns control to this machine if input ever gets stuck on the peer. Without it there is no way to recover except killing hop from another machine. Example: panic_hotkey = \"LeftCtrl+LeftAlt+Escape\""
    )]
    ServerMissingPanicHotkey,

    #[error("config field '{field}' has an unusable path '{raw}': {reason}")]
    PathExpansion {
        field: String,
        raw: String,
        reason: String,
    },

    #[error(
        "config field 'server' = \"{value}\" is not a valid host:port address ({reason}); hop does not implement discovery by name yet, so 'server' must be a literal address and port, for example \"192.168.1.42:24810\" or \"[::1]:24810\" for an IPv6 literal"
    )]
    InvalidServerAddress { value: String, reason: String },
}

/// The raw shape of the TOML file, before validation. Every field is
/// optional here so a missing required field can be reported by name
/// rather than as an opaque deserialization failure.
///
/// Every `Raw*` struct denies unknown fields. Without that, a typo'd key
/// (`pannic_hotkey` for `panic_hotkey`) or a stray section is silently
/// dropped: the file parses, the corresponding value is just `None`, and
/// nothing tells the user their setting never took effect. That is
/// especially dangerous for `panic_hotkey`, the escape hatch back to the
/// local machine, so it must fail loudly instead.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
struct RawPeer {
    id: Option<String>,
    #[serde(default)]
    remap: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDiscovery {
    enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSecurity {
    key_file: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInput {
    panic_hotkey: Option<String>,
    return_edge: Option<String>,
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

        let key_file_raw =
            raw.security
                .and_then(|s| s.key_file)
                .ok_or_else(|| ConfigError::MissingField {
                    field: "security.key_file".into(),
                })?;
        let security = SecurityConfig {
            key_file: expand_config_path(&key_file_raw, "security.key_file")?,
        };

        let input = match raw.input {
            Some(raw_input) => InputConfig {
                panic_hotkey: raw_input.panic_hotkey,
                return_edge: raw_input.return_edge,
            },
            None => InputConfig::default(),
        };

        let config = Config {
            role,
            bind: raw.bind,
            id: raw.id,
            server: raw.server,
            peers,
            layout,
            discovery,
            security,
            input,
        };

        Self::validate(&config)?;

        Ok(config)
    }

    /// Cross-field checks that a single field's own parsing cannot catch.
    /// Each of these passes silently today and only breaks later, at
    /// runtime, far from the config file that caused it: a client with no
    /// server to dial, a server with no peers to ever accept, a layout
    /// edge pointing at a peer id that was never configured. Catching
    /// them here, at load time, is what makes that failure a config
    /// error instead of a mystery.
    fn validate(config: &Config) -> Result<(), ConfigError> {
        match config.role {
            Role::Server => {
                if config.bind.is_none() {
                    return Err(ConfigError::ServerMissingBind);
                }
                if config.peers.is_empty() {
                    return Err(ConfigError::ServerMissingPeers);
                }
                if config.input.panic_hotkey.is_none() {
                    return Err(ConfigError::ServerMissingPanicHotkey);
                }
            }
            Role::Client => {
                if config.id.is_none() {
                    return Err(ConfigError::ClientMissingId);
                }
                let server = match &config.server {
                    Some(server) => server,
                    None => return Err(ConfigError::ClientMissingServer),
                };
                if let Err(reason) = validate_server_address(server) {
                    return Err(ConfigError::InvalidServerAddress {
                        value: server.clone(),
                        reason,
                    });
                }
                match config.input.return_edge.as_deref() {
                    None => return Err(ConfigError::ClientMissingReturnEdge),
                    Some("top" | "bottom" | "left" | "right") => {}
                    Some(other) => {
                        return Err(ConfigError::UnknownReturnEdge {
                            value: other.to_string(),
                        })
                    }
                }
            }
        }

        for (edge, peer_id) in [
            ("top", &config.layout.top),
            ("bottom", &config.layout.bottom),
            ("left", &config.layout.left),
            ("right", &config.layout.right),
        ] {
            if let Some(peer_id) = peer_id {
                if !config.peers.iter().any(|p| &p.id == peer_id) {
                    return Err(ConfigError::LayoutUnknownPeer {
                        edge: edge.to_string(),
                        peer: peer_id.clone(),
                    });
                }
            }
        }

        Ok(())
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

/// Expand `~` and, on Windows, `%VAR%` environment references in a
/// user-supplied path, the way a shell would. Both of the spec's own
/// example configs use exactly this syntax: `key_file =
/// "~/.config/hop/key"` on macOS and `key_file = "%APPDATA%\\hop\\key"`
/// on Windows. Neither means anything to `PathBuf::from` on its own:
/// without expansion `~` becomes a literal directory named `~` under
/// the current working directory, and `hop keygen` reports success
/// while writing the key somewhere the user can never find again.
///
/// Expansion happens once, here, so every consumer of a config path
/// (today, only `[security] key_file`) gets it automatically.
fn expand_config_path(raw: &str, field: &str) -> Result<PathBuf, ConfigError> {
    let percent_expanded = expand_percent_vars(raw, field)?;
    expand_tilde(&percent_expanded, field)
}

/// Expand a leading `~` to the user's home directory (`HOME`, falling
/// back to `USERPROFILE` for the rare case this runs on Windows without
/// `HOME` set). Any other use of `~`, such as `~otheruser/...` naming
/// another account's home, is rejected rather than silently treated as
/// a literal path component, since `std::env` alone cannot resolve it.
fn expand_tilde(raw: &str, field: &str) -> Result<PathBuf, ConfigError> {
    expand_tilde_with(raw, field, |name| std::env::var(name).ok())
}

fn expand_tilde_with(
    raw: &str,
    field: &str,
    home_var: impl Fn(&str) -> Option<String>,
) -> Result<PathBuf, ConfigError> {
    let Some(rest) = raw.strip_prefix('~') else {
        return Ok(PathBuf::from(raw));
    };

    if !(rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\')) {
        return Err(ConfigError::PathExpansion {
            field: field.to_string(),
            raw: raw.to_string(),
            reason: "\"~name/...\" (another user's home directory) is not supported; write an absolute path instead".to_string(),
        });
    }

    let home = home_var("HOME")
        .or_else(|| home_var("USERPROFILE"))
        .ok_or_else(|| ConfigError::PathExpansion {
            field: field.to_string(),
            raw: raw.to_string(),
            reason: "could not expand '~': neither HOME nor USERPROFILE is set in the environment"
                .to_string(),
        })?;

    let mut path = PathBuf::from(home);
    let rest = rest.trim_start_matches(['/', '\\']);
    if !rest.is_empty() {
        path.push(rest);
    }
    Ok(path)
}

/// Expand Windows `%VAR%` environment references, the way `cmd.exe`
/// would. Actually reads the environment only when compiled for
/// Windows; `%VAR%` syntax is a Windows convention, and this project's
/// only user of it, `role = "client"`, only ever runs on Windows (see
/// `run::dispatch`). On other platforms it is passed through unchanged
/// rather than failing to resolve a variable, such as `APPDATA`, that
/// legitimately does not exist there.
#[cfg(target_os = "windows")]
fn expand_percent_vars(raw: &str, field: &str) -> Result<String, ConfigError> {
    expand_percent_vars_with(raw, field, |name| std::env::var(name).ok())
}

#[cfg(not(target_os = "windows"))]
fn expand_percent_vars(raw: &str, _field: &str) -> Result<String, ConfigError> {
    Ok(raw.to_string())
}

/// The actual `%VAR%` substitution, parameterized over the lookup so it
/// can be unit tested on any platform without touching real environment
/// variables. Only called for real (non-test) work on Windows, via
/// `expand_percent_vars` above; `cfg`-gated rather than `#[allow(dead_code)]`
/// so a non-Windows, non-test build does not carry unreachable code.
#[cfg(any(test, target_os = "windows"))]
fn expand_percent_vars_with(
    raw: &str,
    field: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<String, ConfigError> {
    let mut result = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('%') {
        result.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) if end > 0 => {
                let name = &after[..end];
                let value = lookup(name).ok_or_else(|| ConfigError::PathExpansion {
                    field: field.to_string(),
                    raw: raw.to_string(),
                    reason: format!("environment variable '{name}' is not set"),
                })?;
                result.push_str(&value);
                rest = &after[end + 1..];
            }
            _ => {
                // A lone '%' or an empty "%%" is not a variable
                // reference; pass it through literally rather than
                // guessing what the user meant.
                result.push('%');
                rest = after;
            }
        }
    }
    result.push_str(rest);
    Ok(result)
}

/// Validate that `raw` is a literal `host:port` address, the only form
/// `TcpStream::connect` can use today. Returns `Err` naming exactly
/// what is wrong; the caller wraps that into
/// `ConfigError::InvalidServerAddress`.
///
/// Discovery-by-name, the spec's other accepted form (`server =
/// "talhas-mac"`), is not implemented in this build. Accepting it here
/// would let a config load successfully and then fail every single
/// connection attempt forever, visible only as a `warn` log line the
/// user may never see, so it is rejected at load time instead.
fn validate_server_address(raw: &str) -> Result<(), String> {
    let (host, port_str) = if let Some(rest) = raw.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or_else(|| {
            "starts with '[' but has no matching ']'; an IPv6 literal looks like \"[::1]:24810\""
                .to_string()
        })?;
        let port_str = after
            .strip_prefix(':')
            .ok_or_else(|| format!("expected \":<port>\" right after \"]\", got \"{after}\""))?;
        (host, port_str)
    } else if raw.matches(':').count() > 1 {
        return Err(
            "looks like an IPv6 address but has no brackets; wrap it, for example \"[::1]:24810\""
                .to_string(),
        );
    } else if let Some((host, port_str)) = raw.rsplit_once(':') {
        (host, port_str)
    } else {
        return Err("no ':' found; got a bare name with no port".to_string());
    };

    if host.is_empty() {
        return Err("the host part is empty".to_string());
    }

    match port_str.parse::<u16>() {
        Ok(0) | Err(_) => Err(format!("'{port_str}' is not a valid port number (1-65535)")),
        Ok(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolve the home directory the same way `expand_tilde` does, so these
    /// tests pass on Windows too, where `HOME` is unset and `USERPROFILE`
    /// carries the value. Hardcoding `HOME` made three of them fail on the
    /// Windows CI runner while passing on macOS.
    fn test_home() -> String {
        std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .expect("either HOME or USERPROFILE should be set in the test environment")
    }

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
server = "192.168.18.90:24810"

[discovery]
enabled = true

[security]
key_file = "%APPDATA%\\hop\\key"

[input]
return_edge = "bottom"
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
        let home = test_home();
        assert_eq!(
            config.security.key_file,
            PathBuf::from(home).join(".config/hop/key")
        );
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
        assert_eq!(config.server.as_deref(), Some("192.168.18.90:24810"));
        assert!(config.discovery.enabled);
        // `%VAR%` expansion is deliberately a no-op off Windows, so the same
        // config yields a literal path there and a real one on Windows.
        // Asserting the literal everywhere passed on macOS and failed on the
        // Windows runner.
        #[cfg(target_os = "windows")]
        {
            let appdata = std::env::var("APPDATA")
                .expect("APPDATA should be set in the test environment on Windows");
            assert_eq!(
                config.security.key_file,
                PathBuf::from(appdata).join("hop").join("key")
            );
        }
        #[cfg(not(target_os = "windows"))]
        assert_eq!(
            config.security.key_file,
            PathBuf::from("%APPDATA%\\hop\\key")
        );
        assert!(config.peers.is_empty());
        assert_eq!(config.input.return_edge.as_deref(), Some("bottom"));
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

    #[test]
    fn typo_d_field_name_is_a_named_error() {
        // "pannic_hotkey" instead of "panic_hotkey": this must not be
        // silently ignored, since the panic hotkey is the escape hatch
        // back to the local machine.
        let path = write_config(
            r#"
role = "server"
bind = "0.0.0.0:24810"

[[peers]]
id = "pc"

[security]
key_file = "~/.config/hop/key"

[input]
pannic_hotkey = "LeftCtrl+LeftAlt+Escape"
"#,
        );
        let err = Config::load(&path).expect_err("typo'd field name should be rejected");
        fs::remove_file(&path).ok();

        let message = err.to_string();
        assert!(
            message.contains("pannic_hotkey"),
            "error should name the offending field, got: {message}"
        );
        assert!(matches!(err, ConfigError::Toml { .. }));
    }

    #[test]
    fn unknown_section_is_a_named_error() {
        let path = write_config(
            r#"
role = "server"
bind = "0.0.0.0:24810"

[[peers]]
id = "pc"

[security]
key_file = "~/.config/hop/key"

[unexpected_section]
foo = "bar"
"#,
        );
        let err = Config::load(&path).expect_err("unknown section should be rejected");
        fs::remove_file(&path).ok();

        let message = err.to_string();
        assert!(
            message.contains("unexpected_section"),
            "error should name the offending section, got: {message}"
        );
        assert!(matches!(err, ConfigError::Toml { .. }));
    }

    #[test]
    fn server_missing_bind_is_an_error() {
        let path = write_config(
            r#"
role = "server"

[[peers]]
id = "pc"

[security]
key_file = "~/.config/hop/key"
"#,
        );
        let err = Config::load(&path).expect_err("server with no bind should be rejected");
        fs::remove_file(&path).ok();

        assert!(matches!(err, ConfigError::ServerMissingBind));
        let message = err.to_string();
        assert!(message.contains("bind"), "got: {message}");
    }

    #[test]
    fn server_missing_peers_is_an_error() {
        let path = write_config(
            r#"
role = "server"
bind = "0.0.0.0:24810"

[security]
key_file = "~/.config/hop/key"
"#,
        );
        let err = Config::load(&path).expect_err("server with no peers should be rejected");
        fs::remove_file(&path).ok();

        assert!(matches!(err, ConfigError::ServerMissingPeers));
        let message = err.to_string();
        assert!(message.contains("peers"), "got: {message}");
    }

    #[test]
    fn client_missing_id_is_an_error() {
        let path = write_config(
            r#"
role = "client"
server = "192.168.18.90:24810"

[security]
key_file = "%APPDATA%\\hop\\key"
"#,
        );
        let err = Config::load(&path).expect_err("client with no id should be rejected");
        fs::remove_file(&path).ok();

        assert!(matches!(err, ConfigError::ClientMissingId));
        let message = err.to_string();
        assert!(message.contains("id"), "got: {message}");
    }

    #[test]
    fn client_missing_server_is_an_error() {
        let path = write_config(
            r#"
role = "client"
id = "pc"

[security]
key_file = "%APPDATA%\\hop\\key"
"#,
        );
        let err = Config::load(&path).expect_err("client with no server should be rejected");
        fs::remove_file(&path).ok();

        assert!(matches!(err, ConfigError::ClientMissingServer));
        let message = err.to_string();
        assert!(message.contains("server"), "got: {message}");
    }

    #[test]
    fn client_missing_return_edge_is_an_error() {
        // Without an automatic return path, focus can only ever come
        // home through the panic hotkey, which lives on the server, not
        // the client; see CRITICAL 2 in the whole-branch review.
        let path = write_config(
            r#"
role = "client"
id = "pc"
server = "192.168.18.90:24810"

[security]
key_file = "%APPDATA%\\hop\\key"
"#,
        );
        let err =
            Config::load(&path).expect_err("client with no [input] return_edge should be rejected");
        fs::remove_file(&path).ok();

        assert!(matches!(err, ConfigError::ClientMissingReturnEdge));
        let message = err.to_string();
        assert!(message.contains("return_edge"), "got: {message}");
    }

    #[test]
    fn client_unknown_return_edge_is_an_error() {
        let path = write_config(
            r#"
role = "client"
id = "pc"
server = "192.168.18.90:24810"

[security]
key_file = "%APPDATA%\\hop\\key"

[input]
return_edge = "diagonal"
"#,
        );
        let err = Config::load(&path).expect_err("an unknown return_edge value should be rejected");
        fs::remove_file(&path).ok();

        match err {
            ConfigError::UnknownReturnEdge { value } => assert_eq!(value, "diagonal"),
            other => panic!("expected UnknownReturnEdge, got {other:?}"),
        }
    }

    #[test]
    fn server_role_does_not_require_a_return_edge() {
        // return_edge only makes sense on the client; a server config
        // (which SERVER_CONFIG never sets it in) must still load.
        let path = write_config(SERVER_CONFIG);
        let config = Config::load(&path).expect("server config should not require return_edge");
        fs::remove_file(&path).ok();
        assert_eq!(config.input.return_edge, None);
    }

    #[test]
    fn layout_edge_naming_unknown_peer_is_an_error() {
        let path = write_config(
            r#"
role = "server"
bind = "0.0.0.0:24810"

[[peers]]
id = "pc"

[layout]
top = "typo-d-pc"

[security]
key_file = "~/.config/hop/key"

[input]
panic_hotkey = "LeftCtrl+LeftAlt+Escape"
"#,
        );
        let err =
            Config::load(&path).expect_err("layout edge naming unknown peer should be rejected");
        fs::remove_file(&path).ok();

        assert!(matches!(err, ConfigError::LayoutUnknownPeer { .. }));
        let message = err.to_string();
        assert!(
            message.contains("typo-d-pc") && message.contains("top"),
            "error should name the offending edge and peer, got: {message}"
        );
    }

    // --- FINDING 1: `~` and `%VAR%` expansion in key_file ---

    #[test]
    fn tilde_prefixed_path_expands_under_home_directory() {
        let home = test_home();
        let expanded = expand_config_path("~/.config/hop/key", "security.key_file")
            .expect("a tilde-prefixed path should expand");
        assert_eq!(expanded, PathBuf::from(home).join(".config/hop/key"));
    }

    #[test]
    fn bare_tilde_expands_to_home_directory() {
        let home = test_home();
        let expanded = expand_config_path("~", "security.key_file")
            .expect("a bare tilde should expand to the home directory");
        assert_eq!(expanded, PathBuf::from(home));
    }

    #[test]
    fn absolute_path_passes_through_untouched() {
        let expanded = expand_config_path("/etc/hop/key", "security.key_file")
            .expect("an absolute path should pass through");
        assert_eq!(expanded, PathBuf::from("/etc/hop/key"));
    }

    #[test]
    fn relative_path_passes_through_unchanged() {
        // No `~` and no `%...%`: a relative path is left exactly as
        // written, to be resolved against the current directory the
        // same way it always has been.
        let expanded = expand_config_path("hop-key", "security.key_file")
            .expect("a relative path should pass through");
        assert_eq!(expanded, PathBuf::from("hop-key"));
    }

    #[test]
    fn tilde_other_user_home_is_rejected_rather_than_taken_literally() {
        let err = expand_config_path("~talha/key", "security.key_file")
            .expect_err("\"~name/...\" should be rejected, not silently written to a literal path");
        assert!(matches!(err, ConfigError::PathExpansion { .. }));
    }

    #[test]
    fn config_load_expands_tilde_key_file_end_to_end() {
        // The same check as `valid_server_config_parses`'s key_file
        // assertion, but exercised as its own named test since this is
        // the exact reviewer-reported failure from FINDING 1: `hop
        // keygen` claiming success while writing under a literal `~`
        // directory in the current working directory.
        let path = write_config(SERVER_CONFIG);
        let config = Config::load(&path).expect("valid server config should load");
        fs::remove_file(&path).ok();

        let home = test_home();
        assert_ne!(
            config.security.key_file,
            PathBuf::from("~/.config/hop/key"),
            "key_file must not be left as a literal path starting with '~'"
        );
        assert_eq!(
            config.security.key_file,
            PathBuf::from(home).join(".config/hop/key")
        );
    }

    #[test]
    fn percent_var_expands_using_the_given_lookup() {
        let expanded =
            expand_percent_vars_with("%APPDATA%\\hop\\key", "security.key_file", |name| {
                if name == "APPDATA" {
                    Some("C:\\Users\\talha\\AppData\\Roaming".to_string())
                } else {
                    None
                }
            })
            .expect("a known variable should expand");
        assert_eq!(expanded, "C:\\Users\\talha\\AppData\\Roaming\\hop\\key");
    }

    #[test]
    fn percent_var_reports_the_missing_variable_by_name() {
        let err = expand_percent_vars_with("%NOPE%\\key", "security.key_file", |_| None)
            .expect_err("an unset variable should be rejected, not treated as literal text");
        match err {
            ConfigError::PathExpansion { reason, .. } => {
                assert!(reason.contains("NOPE"), "got: {reason}");
            }
            other => panic!("expected PathExpansion, got {other:?}"),
        }
    }

    #[test]
    fn percent_var_wrapper_is_a_noop_off_windows() {
        // `expand_percent_vars` is the OS-dispatching wrapper around
        // `expand_percent_vars_with`. Off Windows it must leave a
        // "%..." string untouched rather than trying, and failing, to
        // look up a variable like APPDATA that only exists on Windows.
        #[cfg(not(target_os = "windows"))]
        {
            let result = expand_percent_vars("%APPDATA%\\hop\\key", "security.key_file")
                .expect("non-Windows platforms must not attempt %VAR% expansion");
            assert_eq!(result, "%APPDATA%\\hop\\key");
        }
    }

    // --- FINDING 2: `server` must be a literal host:port address ---

    #[test]
    fn bare_discovery_name_as_server_is_rejected() {
        let path = write_config(
            r#"
role = "client"
id = "pc"
server = "talhas-mac"

[security]
key_file = "%APPDATA%\\hop\\key"

[input]
return_edge = "bottom"
"#,
        );
        let err = Config::load(&path)
            .expect_err("a bare discovery name should be rejected: discovery is not implemented");
        fs::remove_file(&path).ok();

        let message = err.to_string();
        match &err {
            ConfigError::InvalidServerAddress { value, .. } => {
                assert_eq!(value, "talhas-mac");
            }
            other => panic!("expected InvalidServerAddress, got {other:?}"),
        }
        assert!(
            message.contains("discovery"),
            "error should explain that discovery by name is not implemented, got: {message}"
        );
    }

    #[test]
    fn host_port_server_is_accepted() {
        let path = write_config(CLIENT_CONFIG);
        let config = Config::load(&path).expect("a host:port server address should be accepted");
        fs::remove_file(&path).ok();
        assert_eq!(config.server.as_deref(), Some("192.168.18.90:24810"));
    }

    #[test]
    fn ipv6_literal_server_is_accepted() {
        let path = write_config(
            r#"
role = "client"
id = "pc"
server = "[::1]:24810"

[security]
key_file = "%APPDATA%\\hop\\key"

[input]
return_edge = "bottom"
"#,
        );
        let config =
            Config::load(&path).expect("a bracketed IPv6 literal with a port should be accepted");
        fs::remove_file(&path).ok();
        assert_eq!(config.server.as_deref(), Some("[::1]:24810"));
    }

    #[test]
    fn server_with_bad_port_is_rejected() {
        let path = write_config(
            r#"
role = "client"
id = "pc"
server = "192.168.18.90:notaport"

[security]
key_file = "%APPDATA%\\hop\\key"

[input]
return_edge = "bottom"
"#,
        );
        let err = Config::load(&path).expect_err("a non-numeric port should be rejected");
        fs::remove_file(&path).ok();
        assert!(matches!(err, ConfigError::InvalidServerAddress { .. }));
    }

    #[test]
    fn unbracketed_ipv6_server_is_rejected_with_a_bracket_hint() {
        let path = write_config(
            r#"
role = "client"
id = "pc"
server = "::1:24810"

[security]
key_file = "%APPDATA%\\hop\\key"

[input]
return_edge = "bottom"
"#,
        );
        let err = Config::load(&path)
            .expect_err("an unbracketed address with multiple colons should be rejected");
        fs::remove_file(&path).ok();
        let message = err.to_string();
        assert!(
            message.contains("bracket") || message.contains("["),
            "error should hint at brackets for IPv6, got: {message}"
        );
    }

    // --- FINDING 5: panic_hotkey is required for role = "server" ---

    #[test]
    fn server_missing_panic_hotkey_is_an_error() {
        let path = write_config(
            r#"
role = "server"
bind = "0.0.0.0:24810"

[[peers]]
id = "pc"

[security]
key_file = "~/.config/hop/key"
"#,
        );
        let err =
            Config::load(&path).expect_err("a server config with no panic_hotkey must be rejected");
        fs::remove_file(&path).ok();

        assert!(matches!(err, ConfigError::ServerMissingPanicHotkey));
        let message = err.to_string();
        assert!(
            message.contains("panic_hotkey") && message.contains("emergency"),
            "error should explain why panic_hotkey is required, got: {message}"
        );
    }

    #[test]
    fn server_with_panic_hotkey_is_accepted() {
        // SERVER_CONFIG already sets panic_hotkey; this just names the
        // positive case explicitly alongside the negative one above.
        let path = write_config(SERVER_CONFIG);
        let config = Config::load(&path).expect("a server config with panic_hotkey should load");
        fs::remove_file(&path).ok();
        assert_eq!(
            config.input.panic_hotkey.as_deref(),
            Some("LeftCtrl+LeftAlt+Escape")
        );
    }

    #[test]
    fn client_without_panic_hotkey_is_still_fine() {
        // Only the server ever captures input, so only the server needs
        // the emergency escape; CLIENT_CONFIG never sets panic_hotkey
        // and must still load.
        let path = write_config(CLIENT_CONFIG);
        let config = Config::load(&path).expect("a client config needs no panic_hotkey");
        fs::remove_file(&path).ok();
        assert_eq!(config.input.panic_hotkey, None);
    }
}
