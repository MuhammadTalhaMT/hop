//! The two things `hop` actually does once it has a config: `keygen`,
//! which creates a shared key, and `run`, which loads a config and key and
//! drives this machine's configured role.
//!
//! Every error here names the file, field, or address at fault. A config
//! file, a key file, and a bind address are all user input, and the
//! project's rule (see `CLAUDE.md`) is that user input never panics: it
//! produces a `RunError` that says what to fix.

#[cfg(target_os = "macos")]
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "macos")]
use std::sync::Arc;
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;

use hop::config::{Config, ConfigError};
use hop::keymap;
#[cfg(target_os = "macos")]
use hop_core::{send_release_all, server_handshake, Action, Capturer, Control, InputEvent};
use hop_proto::{SharedKey, Usage};

/// Everything that can go wrong running `hop keygen` or `hop run`.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(
        "keygen needs either --out <path> or --config <path> pointing at a config with [security] key_file set"
    )]
    KeygenNeedsDestination,

    #[error(
        "key file {0} already exists; refusing to overwrite it without --force (a mismatched key on the two machines produces a baffling authentication failure, so overwriting one by accident is worse than failing here)"
    )]
    KeyFileExists(PathBuf),

    #[error("could not create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not write key file {path}: {source}")]
    KeyWrite {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not read key file {path}: {source}")]
    KeyRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "key file {path} does not contain a valid key ({reason}); regenerate it with `hop keygen` and copy the new file to both machines"
    )]
    KeyDecode { path: PathBuf, reason: String },

    #[error("system RNG unavailable while generating a key")]
    Random,

    #[error(
        "input.panic_hotkey = \"{raw}\" is not a valid hotkey: unknown key name '{bad}'; expected names like \"LeftCtrl\", \"LeftAlt\", \"Escape\" joined with '+', for example \"LeftCtrl+LeftAlt+Escape\""
    )]
    HotkeyParse { raw: String, bad: String },

    #[error(
        "role = \"{role}\" requires '{field}', which the config passed validation without; this is a bug in Config::load, not something a user can fix by editing the config"
    )]
    MissingRoleField {
        role: &'static str,
        field: &'static str,
    },

    #[error("role = \"{role}\" is not supported on {platform}: {detail}")]
    WrongRoleForPlatform {
        role: &'static str,
        platform: &'static str,
        detail: &'static str,
    },

    #[cfg(target_os = "macos")]
    #[error("failed to start macOS input capture: {0}")]
    Capture(String),

    #[cfg(target_os = "macos")]
    #[error("failed to bind {addr}: {source}")]
    Bind {
        addr: String,
        #[source]
        source: std::io::Error,
    },

    #[cfg(target_os = "macos")]
    #[error(
        "role = \"server\" requires a [layout] entry naming the edge input crosses to reach the peer (for example `top = \"pc\"` if the peer's monitor sits above this Mac); with none configured, the cursor could never hand focus over"
    )]
    ServerMissingLayoutEdge,
}

/// `hop keygen`: generate a fresh shared key and write it, base64 encoded,
/// to `out` if given, or to the `[security] key_file` path from the config
/// at `config_path` otherwise. Refuses to overwrite an existing key file
/// unless `force` is set, since silently replacing a key breaks the
/// pairing with the other machine with no explanation.
pub fn keygen(config_path: Option<&Path>, out: Option<&Path>, force: bool) -> Result<(), RunError> {
    let path: PathBuf = match out {
        Some(p) => p.to_path_buf(),
        None => {
            let config_path = config_path.ok_or(RunError::KeygenNeedsDestination)?;
            let config = Config::load(config_path)?;
            config.security.key_file
        }
    };

    if path.exists() && !force {
        return Err(RunError::KeyFileExists(path));
    }

    let key = SharedKey::generate().map_err(|_| RunError::Random)?;
    let encoded = BASE64.encode(key.as_bytes());

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|source| RunError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }

    fs::write(&path, encoded.as_bytes()).map_err(|source| RunError::KeyWrite {
        path: path.clone(),
        source,
    })?;

    set_key_file_permissions(&path)?;

    println!("wrote a new key to {}", path.display());
    println!(
        "copy this exact file to the other machine's key_file path. A mismatched key on the \
         two machines produces an authentication failure with no other symptom, so use the \
         same file, not a freshly generated one, on both sides."
    );

    Ok(())
}

/// Restrict the key file to owner read/write on Unix. A no-op on any other
/// platform: Windows has no equivalent bit, and the file still lives under
/// the user's own profile directory there.
#[cfg(unix)]
fn set_key_file_permissions(path: &Path) -> Result<(), RunError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)
        .map_err(|source| RunError::KeyWrite {
            path: path.to_path_buf(),
            source,
        })?
        .permissions();
    perms.set_mode(0o600);
    fs::set_permissions(path, perms).map_err(|source| RunError::KeyWrite {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(not(unix))]
fn set_key_file_permissions(_path: &Path) -> Result<(), RunError> {
    Ok(())
}

/// Read and base64-decode the shared key at `path`.
fn load_key(path: &Path) -> Result<SharedKey, RunError> {
    let text = fs::read_to_string(path).map_err(|source| RunError::KeyRead {
        path: path.to_path_buf(),
        source,
    })?;
    let bytes = BASE64
        .decode(text.trim())
        .map_err(|error| RunError::KeyDecode {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
    let array: [u8; 32] = bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| RunError::KeyDecode {
            path: path.to_path_buf(),
            reason: format!("expected a 32-byte key, got {} bytes", bytes.len()),
        })?;
    Ok(SharedKey::from_bytes(array))
}

/// Parse `[input] panic_hotkey` if the config sets it. An empty result
/// means no hotkey is configured; a hotkey that fails to parse is a
/// startup error, never a failure deferred to the moment it is needed.
fn parse_panic_hotkey(config: &Config) -> Result<Vec<Usage>, RunError> {
    match &config.input.panic_hotkey {
        Some(raw) => keymap::parse_combo(raw).map_err(|bad| RunError::HotkeyParse {
            raw: raw.clone(),
            bad,
        }),
        None => Ok(Vec::new()),
    }
}

/// `hop run`: load the config and key, validate the panic hotkey, and
/// drive this machine's configured role forever.
pub async fn run(config_path: &Path) -> Result<(), RunError> {
    let config = Config::load(config_path)?;
    let panic_hotkey = parse_panic_hotkey(&config)?;
    let key = load_key(&config.security.key_file)?;

    tracing::info!(role = ?config.role, "config and key loaded");

    dispatch(config, key, panic_hotkey).await
}

#[cfg(target_os = "macos")]
async fn dispatch(
    config: Config,
    key: SharedKey,
    panic_hotkey: Vec<Usage>,
) -> Result<(), RunError> {
    match config.role {
        hop::config::Role::Server => run_server(config, key, panic_hotkey).await,
        hop::config::Role::Client => Err(RunError::WrongRoleForPlatform {
            role: "client",
            platform: "macOS",
            detail: "hop's macOS build only implements the server role (input capture); run role = \"client\" on the Windows machine instead",
        }),
    }
}

#[cfg(target_os = "windows")]
async fn dispatch(
    config: Config,
    key: SharedKey,
    _panic_hotkey: Vec<Usage>,
) -> Result<(), RunError> {
    match config.role {
        hop::config::Role::Client => run_client(config, key).await,
        hop::config::Role::Server => Err(RunError::WrongRoleForPlatform {
            role: "server",
            platform: "Windows",
            detail: "hop's Windows build only implements the client role (input injection); run role = \"server\" on the Mac instead",
        }),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
async fn dispatch(
    config: Config,
    _key: SharedKey,
    _panic_hotkey: Vec<Usage>,
) -> Result<(), RunError> {
    let role = match config.role {
        hop::config::Role::Server => "server",
        hop::config::Role::Client => "client",
    };
    Err(RunError::WrongRoleForPlatform {
        role,
        platform: std::env::consts::OS,
        detail:
            "hop only runs on macOS (server, input capture) and Windows (client, input injection)",
    })
}

#[cfg(target_os = "windows")]
async fn run_client(config: Config, key: SharedKey) -> Result<(), RunError> {
    let id = config.id.clone().ok_or(RunError::MissingRoleField {
        role: "client",
        field: "id",
    })?;
    let server = config.server.clone().ok_or(RunError::MissingRoleField {
        role: "client",
        field: "server",
    })?;
    // `Config::load` already requires `input.return_edge` to be present
    // and one of the four edge names for role = "client" (see
    // CRITICAL 2 in the whole-branch review), so a `None` here means
    // that validation was bypassed, not that the user forgot to set it.
    let return_edge = config
        .input
        .return_edge
        .as_deref()
        .and_then(hop_platform::windows::ReturnEdge::parse)
        .ok_or(RunError::MissingRoleField {
            role: "client",
            field: "input.return_edge",
        })?;

    let mut injector = hop_platform::windows::WindowsInjector::new(Some(return_edge));
    let mut supervisor = hop_core::ClientSupervisor::new(server.clone(), key, id);

    tracing::info!(
        server = %server,
        ?return_edge,
        "starting hop client; reconnecting forever on any link loss"
    );
    supervisor.run(&mut injector).await
}

/// Wraps a capturer, passing every event through unchanged, while watching
/// key events for the moment every key in `combo` is simultaneously held.
/// When that happens it flips `triggered`, which the connection loop below
/// checks after every drain of the capturer and turns into a locally
/// driven [`Action::ReleaseAll`].
///
/// Deliberately does not swallow the triggering keystrokes: buffering keys
/// to find out later whether they were part of a combo would add latency
/// to every single keypress, and letting the combo's own keys reach the
/// peer for the instant before the release-all follows is a far smaller
/// cost than that.
#[cfg(target_os = "macos")]
struct HotkeyWatcher<'a, C> {
    inner: &'a mut C,
    combo: &'a HashSet<Usage>,
    held: HashSet<Usage>,
    triggered: Arc<AtomicBool>,
}

#[cfg(target_os = "macos")]
impl<'a, C: Capturer> Capturer for HotkeyWatcher<'a, C> {
    fn poll(&mut self) -> Option<InputEvent> {
        let event = self.inner.poll();
        if let Some(InputEvent::Key { usage, pressed }) = event {
            if pressed {
                self.held.insert(usage);
            } else {
                self.held.remove(&usage);
            }
            if !self.combo.is_empty() && self.combo.is_subset(&self.held) {
                self.triggered.store(true, Ordering::SeqCst);
            }
        }
        event
    }
}

/// The single screen edge this server watches for a crossing into the
/// peer, taken from `[layout]` rather than assumed. `[layout]` accepts a
/// peer id per edge, in principle allowing a future multi-peer routing
/// where different edges send focus to different machines, but today's
/// server drives exactly one `MacCapturer` watching exactly one edge
/// (`MacCapturer::start` is called once, before any peer has connected),
/// so only the first configured edge in this fixed order is honored. That
/// matches this project's actual deployment, a single PC whose monitors
/// sit above the Mac, configured as `top = "pc"`; there is nothing here
/// that assumes top specifically.
#[cfg(target_os = "macos")]
fn configured_edge(layout: &hop::config::Layout) -> Option<hop_platform::macos::Edge> {
    use hop_platform::macos::Edge;
    if layout.top.is_some() {
        Some(Edge::Top)
    } else if layout.right.is_some() {
        Some(Edge::Right)
    } else if layout.bottom.is_some() {
        Some(Edge::Bottom)
    } else if layout.left.is_some() {
        Some(Edge::Left)
    } else {
        None
    }
}

/// Server role: capture locally, listen for a client, and forward input to
/// whichever client is connected. A client is expected to come and go (a
/// sleep, a lock, a network blip), so losing one returns to listening
/// instead of exiting.
#[cfg(target_os = "macos")]
async fn run_server(
    config: Config,
    key: SharedKey,
    panic_hotkey: Vec<Usage>,
) -> Result<(), RunError> {
    let bind = config.bind.clone().ok_or(RunError::MissingRoleField {
        role: "server",
        field: "bind",
    })?;
    let edge = configured_edge(&config.layout).ok_or(RunError::ServerMissingLayoutEdge)?;

    // Built before `MacCapturer::start` so the same combo can be handed
    // to both the capturer (for its own unconditional escape hatch, see
    // CRITICAL 1) and `handle_client` below (for the sanctioned,
    // connection-aware path through `Control`).
    let panic_combo: HashSet<Usage> = panic_hotkey.into_iter().collect();

    let mut capturer = hop_platform::macos::MacCapturer::start(edge, panic_combo.clone())
        .map_err(|error| RunError::Capture(error.to_string()))?;
    tracing::info!(?edge, "macOS input capture started");

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|source| RunError::Bind {
            addr: bind.clone(),
            source,
        })?;
    tracing::info!(addr = %bind, "listening for a client");

    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(v) => v,
            Err(error) => {
                tracing::warn!(%error, "accept failed; retrying shortly");
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };
        tracing::info!(%peer_addr, "accepted a connection");

        // Nagle's algorithm buffers small outbound writes waiting for the
        // previous one's ACK, which is the pathological worst case for a
        // continuous stream of small mouse-motion packets: each can sit in
        // the kernel for tens of milliseconds. A working-but-laggy link
        // beats no link at all, so a failure here is logged and the
        // connection proceeds rather than being torn down over it.
        if let Err(error) = stream.set_nodelay(true) {
            tracing::debug!(%error, "failed to set TCP_NODELAY on the accepted socket");
        }

        handle_client(stream, &key, &config.peers, &mut capturer, &panic_combo).await;

        tracing::info!("client disconnected; returning to listening");
    }
}

/// Run one client connection to completion: handshake, then forward
/// captured input until the link drops or goes silent for too long. Never
/// itself retries; `run_server`'s accept loop does that by construction.
#[cfg(target_os = "macos")]
async fn handle_client(
    stream: tokio::net::TcpStream,
    key: &SharedKey,
    peers: &[hop::config::PeerConfig],
    capturer: &mut hop_platform::macos::MacCapturer,
    panic_combo: &HashSet<Usage>,
) {
    let (reader, mut writer, session, peer_id) = match server_handshake(stream, key).await {
        Ok(v) => v,
        Err(error) => {
            tracing::warn!(%error, "handshake failed");
            return;
        }
    };

    let remap = match peers.iter().find(|peer| peer.id == peer_id) {
        Some(peer) => peer.remap.clone(),
        None => {
            tracing::warn!(peer_id = %peer_id, "connection from a peer id not listed in [[peers]]; refusing");
            return;
        }
    };

    tracing::info!(peer_id = %peer_id, ?session, "handshake complete");

    let remote_flag = capturer.remote_flag();
    // Set now that a peer genuinely exists, and cleared below however this
    // connection ends, including every early `break`: this is the other
    // half of CRITICAL 1's fix. Edge detection in the tap callback checks
    // this before it will ever start a crossing (see
    // `should_begin_crossing`), so a crossing with nobody connected is a
    // no-op instead of a trap.
    let peer_connected_flag = capturer.peer_connected_flag();
    peer_connected_flag.store(true, Ordering::Relaxed);
    let mut control = Control::new();
    let triggered = Arc::new(AtomicBool::new(false));
    let mut watched = HotkeyWatcher {
        inner: capturer,
        combo: panic_combo,
        held: HashSet::new(),
        triggered: triggered.clone(),
    };

    // `TransportReader::recv` is not cancel safe (see its doc comment), so
    // it gets its own task that always runs it to completion, exactly like
    // `ClientSupervisor::run_connection`, rather than being raced directly
    // against the poll ticker below.
    let (tx, mut rx) =
        tokio::sync::mpsc::channel::<Result<hop_proto::Message, hop_core::TransportError>>(8);
    let reader_task = tokio::spawn(async move {
        let mut reader = reader;
        loop {
            let result = reader.recv().await;
            let ended = result.is_err();
            if tx.send(result).await.is_err() || ended {
                break;
            }
        }
    });

    let heartbeat_interval = Duration::from_secs(1);
    let death_timeout = Duration::from_secs(3);
    let now = Instant::now();
    let mut liveness = hop_core::Liveness::new(now, heartbeat_interval, death_timeout);
    let mut poll_ticker = tokio::time::interval(Duration::from_millis(15));

    'connection: loop {
        tokio::select! {
            received = rx.recv() => {
                match received {
                    Some(Ok(message)) => {
                        liveness.record_activity(Instant::now());
                        if let hop_proto::Message::Release = message {
                            if control.on_release_requested() == Action::ReleaseAll
                                && send_release_all(&mut writer).await.is_err()
                            {
                                tracing::warn!("failed to send release-all; disconnecting");
                                break 'connection;
                            }
                            remote_flag.store(control.focus() == hop_core::Focus::Remote, Ordering::Relaxed);
                        }
                    }
                    Some(Err(error)) => {
                        tracing::warn!(%error, "transport error; disconnecting");
                        break 'connection;
                    }
                    None => {
                        tracing::warn!("reader task ended unexpectedly; disconnecting");
                        break 'connection;
                    }
                }
            }
            _ = poll_ticker.tick() => {
                let now = Instant::now();
                if liveness.is_dead(now) {
                    tracing::warn!(timeout = ?death_timeout, "no activity from the client within the timeout; disconnecting");
                    break 'connection;
                }

                if let Err(error) = hop_core::pump_server(&mut writer, &mut watched, &mut control, &remap).await {
                    tracing::warn!(%error, "failed to forward input; disconnecting");
                    break 'connection;
                }
                remote_flag.store(control.focus() == hop_core::Focus::Remote, Ordering::Relaxed);

                if triggered.swap(false, Ordering::SeqCst) {
                    tracing::info!("panic hotkey pressed; returning focus to this machine");
                    let action = control.on_panic_hotkey();
                    remote_flag.store(control.focus() == hop_core::Focus::Remote, Ordering::Relaxed);
                    if action == Action::ReleaseAll && send_release_all(&mut writer).await.is_err() {
                        tracing::warn!("failed to send release-all after the panic hotkey; disconnecting");
                        break 'connection;
                    }
                }

                if liveness.should_send_heartbeat(now) {
                    if let Err(error) = writer.send(&hop_proto::Message::Heartbeat).await {
                        tracing::warn!(%error, "failed to send heartbeat; disconnecting");
                        break 'connection;
                    }
                    liveness.record_heartbeat_sent(Instant::now());
                }
            }
        }
    }

    // However this connection ended, local input must resume immediately:
    // this is the same guarantee `ClientSupervisor` gives on the other
    // side, applied here to the tap's suppression flag rather than to
    // injected keys.
    remote_flag.store(false, Ordering::Relaxed);
    // And no crossing can begin again until a fresh peer actually
    // connects; see the comment where this was set above.
    peer_connected_flag.store(false, Ordering::Relaxed);

    // Tear down both transport halves together; see
    // `ClientSupervisor::run_connection`'s comment on why dropping only one
    // leaves the other blocked forever on a half-open socket.
    reader_task.abort();
    let _ = reader_task.await;
    drop(writer);
}

#[cfg(test)]
mod tests {
    use super::*;
    // Explicit, not just inherited from `use super::*`: the top-level
    // `Ordering` import a few lines up is `#[cfg(target_os = "macos")]`,
    // so this module needs its own unconditional import to compile its
    // `temp_path` helper below on every target, Windows included. This
    // was the exact gap that made `cargo check --workspace --target
    // x86_64-pc-windows-msvc` pass locally while CI's Windows test job
    // failed to compile: plain `check` never compiles `#[cfg(test)]`
    // code, only `--all-targets` does.
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_path(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "hop-run-test-{}-{n}-{nanos}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn keygen_writes_a_base64_key_and_refuses_to_overwrite() {
        let path = temp_path("key");
        keygen(None, Some(&path), false).expect("first keygen should succeed");
        assert!(path.exists());

        let loaded = load_key(&path).expect("the file keygen wrote should load back");

        let err = keygen(None, Some(&path), false)
            .expect_err("keygen without --force must refuse to overwrite an existing key");
        assert!(matches!(err, RunError::KeyFileExists(_)));

        // --force does overwrite, and the new key differs from the old one.
        keygen(None, Some(&path), true).expect("keygen with --force should overwrite");
        let reloaded = load_key(&path).expect("the overwritten file should still load");
        assert_ne!(loaded.as_bytes(), reloaded.as_bytes());

        fs::remove_file(&path).ok();
    }

    #[cfg(unix)]
    #[test]
    fn keygen_sets_owner_only_permissions_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_path("key-perms");
        keygen(None, Some(&path), false).expect("keygen should succeed");
        let mode = fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn keygen_without_out_or_config_is_a_named_error() {
        let err = keygen(None, None, false).expect_err("keygen needs a destination");
        assert!(matches!(err, RunError::KeygenNeedsDestination));
    }

    #[test]
    fn load_key_rejects_a_non_base64_file() {
        let path = temp_path("bad-key");
        fs::write(&path, b"not base64 at all! ***").unwrap();
        let err = load_key(&path).expect_err("garbage should not decode");
        fs::remove_file(&path).ok();
        assert!(matches!(err, RunError::KeyDecode { .. }));
    }

    #[test]
    fn load_key_rejects_the_wrong_length() {
        let path = temp_path("short-key");
        fs::write(&path, BASE64.encode([1u8; 16])).unwrap();
        let err = load_key(&path).expect_err("16 bytes is not a valid key length");
        fs::remove_file(&path).ok();
        assert!(matches!(err, RunError::KeyDecode { .. }));
    }

    #[test]
    fn load_key_reports_a_missing_file_by_name() {
        let path = temp_path("does-not-exist");
        let err = load_key(&path).expect_err("a missing key file should be a named error");
        assert!(matches!(err, RunError::KeyRead { .. }));
    }

    /// Builds a config to exercise `parse_panic_hotkey` in isolation.
    /// Uses `role = "client"` rather than `"server"`, since a server
    /// config now requires `[input] panic_hotkey` to load at all (see
    /// `ConfigError::ServerMissingPanicHotkey`); a client never captures
    /// input and so never requires one, letting the `None` case here
    /// still exercise a config that loads successfully.
    fn config_with_hotkey(raw: Option<&str>) -> Config {
        let hotkey_line = match raw {
            Some(raw) => format!("panic_hotkey = \"{raw}\"\n"),
            None => String::new(),
        };
        let text = format!(
            "role = \"client\"\nid = \"pc\"\nserver = \"192.168.18.90:24810\"\n\n[security]\nkey_file = \"/tmp/hop-key\"\n\n[input]\nreturn_edge = \"bottom\"\n{hotkey_line}"
        );
        let path = temp_path("hotkey-config.toml");
        fs::write(&path, text).unwrap();
        let config = Config::load(&path).expect("config should parse");
        fs::remove_file(&path).ok();
        config
    }

    #[test]
    fn no_panic_hotkey_configured_parses_to_empty() {
        let config = config_with_hotkey(None);
        assert_eq!(parse_panic_hotkey(&config).unwrap(), Vec::new());
    }

    #[test]
    fn a_valid_panic_hotkey_parses_to_its_keys() {
        let config = config_with_hotkey(Some("LeftCtrl+LeftAlt+Escape"));
        assert_eq!(
            parse_panic_hotkey(&config).unwrap(),
            vec![Usage::LEFT_CTRL, Usage::LEFT_ALT, Usage::ESCAPE]
        );
    }

    #[test]
    fn an_unparseable_panic_hotkey_is_a_named_startup_error() {
        let config = config_with_hotkey(Some("LeftCtrl+NotAKey"));
        let err = parse_panic_hotkey(&config).expect_err("bad hotkey must fail to parse");
        match err {
            RunError::HotkeyParse { raw, bad } => {
                assert_eq!(raw, "LeftCtrl+NotAKey");
                assert_eq!(bad, "NotAKey");
            }
            other => panic!("expected HotkeyParse, got {other:?}"),
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn configured_edge_reads_top_for_this_projects_real_deployment() {
        // The PC's monitors sit above the Mac in the deployment this
        // project actually ships for, so a config with only `top` set
        // must resolve to `Edge::Top`.
        let layout = hop::config::Layout {
            top: Some("pc".to_string()),
            ..Default::default()
        };
        assert_eq!(
            configured_edge(&layout),
            Some(hop_platform::macos::Edge::Top)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn configured_edge_is_none_with_no_layout_configured() {
        let layout = hop::config::Layout::default();
        assert_eq!(configured_edge(&layout), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn configured_edge_prefers_top_when_more_than_one_edge_is_set() {
        let layout = hop::config::Layout {
            top: Some("pc".to_string()),
            left: Some("laptop".to_string()),
            ..Default::default()
        };
        assert_eq!(
            configured_edge(&layout),
            Some(hop_platform::macos::Edge::Top)
        );
    }
}
