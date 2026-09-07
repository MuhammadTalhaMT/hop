// Only Windows updates itself, so on any other host every function below
// is unreachable from `update`. They are still compiled and still tested
// there, which is the point: the version comparison, the release parsing
// and the file swap are all exercised on the Mac this project is
// developed on, where the Windows path never runs.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

//! Updating hop in place from its GitHub releases.
//!
//! This exists because the alternative was the author copying a fresh
//! `hop.exe` onto the PC by hand after every fix, which is the kind of
//! chore that stops a tool being used.
//!
//! Only the PC updates itself. The Mac builds locally through
//! `update-mac.sh`, and it has to: macOS ties Accessibility permission to
//! the signing identity, so a binary built by CI (which has no access to
//! the author's certificate) would arrive as a different program in
//! macOS's eyes and silently lose the permission the event tap depends
//! on. Overwriting the Mac binary from CI would break input capture
//! entirely, which is why `update` refuses to do it.
//!
//! Everything that decides ANYTHING is pure and tested below: parsing
//! versions, comparing them, and picking this platform's asset out of a
//! release. The network and the file swap are the thin parts.

use std::path::{Path, PathBuf};

/// The repository releases are published from.
pub const REPO: &str = "MuhammadTalhaMT/hop";

/// Refuse anything absurdly large as a sanity check on what we are about
/// to write over our own executable. A hop build is a few megabytes.
const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("could not reach GitHub: {0}")]
    Network(String),
    #[error("GitHub's reply was not what this build expects: {0}")]
    Malformed(String),
    #[error("this release has no {0} build attached to it")]
    NoAssetForPlatform(&'static str),
    #[error("the download was {got} bytes, which is not a plausible hop build")]
    ImplausibleSize { got: u64 },
    #[error("could not work out where this hop is installed: {0}")]
    NoInstallPath(std::io::Error),
    #[error("could not replace {path}: {source}")]
    Replace {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Only ever constructed off Windows, which is why it carries the
    /// allow: on Windows this variant is unreachable by construction.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    #[error(
        "hop updates itself only on Windows. On the Mac, Accessibility permission is tied to \
         the signing certificate, and a build downloaded from CI is not signed with yours, so \
         replacing this binary would silently break input capture. Rebuild locally instead: \
         ./update-mac.sh"
    )]
    NotOnMacos,
}

/// A three part version, which is all hop's tags ever are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// Parses `1.2.3` or `v1.2.3`. `None` for anything else, including a
    /// prerelease suffix: hop has no use for one, and a tag it cannot
    /// read exactly is a tag it should leave alone rather than guess at
    /// and then overwrite a working binary because of.
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.strip_prefix('v').unwrap_or(raw);
        let mut parts = raw.split('.');
        let mut next = || parts.next()?.parse::<u64>().ok();
        let (major, minor, patch) = (next()?, next()?, next()?);
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }

    /// This build's own version, from Cargo.
    pub fn current() -> Option<Self> {
        Self::parse(env!("CARGO_PKG_VERSION"))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The name the release workflow gives this platform's binary.
pub fn asset_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "hop.exe"
    } else {
        "hop"
    }
}

/// A release, reduced to the two things hop cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub download_url: String,
}

/// Pulls this platform's release out of GitHub's JSON.
///
/// Separate from the request that fetched it so the shape of the reply is
/// tested directly, against a real recorded payload, rather than only
/// against a live network.
pub fn release_from_json(body: &str, asset: &str) -> Result<Release, UpdateError> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| UpdateError::Malformed(e.to_string()))?;

    let tag = value
        .get("tag_name")
        .and_then(|t| t.as_str())
        .ok_or_else(|| UpdateError::Malformed("no tag_name".into()))?;
    let version = Version::parse(tag)
        .ok_or_else(|| UpdateError::Malformed(format!("tag {tag} is not a version")))?;

    let download_url = value
        .get("assets")
        .and_then(|a| a.as_array())
        .ok_or_else(|| UpdateError::Malformed("no assets".into()))?
        .iter()
        .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(asset))
        .and_then(|a| a.get("browser_download_url"))
        .and_then(|u| u.as_str())
        .ok_or(UpdateError::NoAssetForPlatform(if cfg!(windows) {
            "Windows"
        } else {
            "macOS"
        }))?
        .to_string();

    Ok(Release {
        version,
        download_url,
    })
}

/// Whether `available` is worth installing over `current`.
///
/// Strictly newer, never merely different: a release that has been pulled
/// and re-cut lower, or a local build ahead of what is published, must
/// not drag the binary backwards.
pub fn is_upgrade(current: Version, available: Version) -> bool {
    available > current
}

/// Splits `https://host/path` into its host and its path.
///
/// Pure, and tested below, because the platform's HTTP call takes the two
/// separately and handing it the wrong halves would mean fetching from
/// the wrong host entirely. Only `https` is accepted: hop is about to
/// overwrite its own executable with whatever comes back, so plain HTTP
/// is not a thing to be tolerated for convenience.
pub fn split_https_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    if host.is_empty() || host.contains('@') {
        return None;
    }
    Some((host.to_string(), path.to_string()))
}

/// Fetches a URL through the platform's own HTTP stack.
#[cfg(target_os = "windows")]
fn http_get(url: &str, limit: usize) -> Result<Vec<u8>, UpdateError> {
    let (host, path) =
        split_https_url(url).ok_or_else(|| UpdateError::Malformed(format!("bad url {url}")))?;
    hop_platform::windows::http::get(&host, &path, "hop-updater", limit)
        .map_err(UpdateError::Network)
}

/// Asks GitHub what the newest release is.
#[cfg(target_os = "windows")]
fn fetch_latest() -> Result<Release, UpdateError> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = http_get(&url, 4 * 1024 * 1024)?;
    let body = String::from_utf8(body)
        .map_err(|_| UpdateError::Malformed("the reply was not text".into()))?;
    release_from_json(&body, asset_name())
}

/// Downloads a release binary into memory.
///
/// Held in memory rather than streamed to disk so that a download which
/// fails halfway can never leave a truncated binary anywhere near the
/// path hop is about to run from. The size cap is what makes holding it
/// in memory safe.
#[cfg(target_os = "windows")]
fn download(url: &str) -> Result<Vec<u8>, UpdateError> {
    let bytes = http_get(url, MAX_DOWNLOAD_BYTES as usize)?;
    if !is_plausible_build(bytes.len() as u64) {
        return Err(UpdateError::ImplausibleSize {
            got: bytes.len() as u64,
        });
    }
    Ok(bytes)
}

/// Whether a download is big enough to be a hop build rather than an
/// error page. GitHub serving HTML where a binary was expected is the
/// realistic failure, and writing that over hop.exe would leave the user
/// with nothing that runs.
pub fn is_plausible_build(bytes: u64) -> bool {
    (500_000..=MAX_DOWNLOAD_BYTES).contains(&bytes)
}

/// Puts `bytes` in place of the executable at `target`.
///
/// Windows will not let a running executable be overwritten, but it will
/// let it be RENAMED, which is the whole trick: the running binary is
/// moved aside to `.old` and the new one takes its name. The running
/// process carries on from the renamed file, and the next start deletes
/// the leftover.
///
/// The new file is written next to the target, never to a temporary
/// directory, so the final step is a rename within one filesystem and
/// cannot half succeed.
pub fn replace_executable(target: &Path, bytes: &[u8]) -> Result<(), UpdateError> {
    let staged = target.with_extension("new");
    let previous = target.with_extension("old");

    let fail = |source: std::io::Error| UpdateError::Replace {
        path: target.to_path_buf(),
        source,
    };

    std::fs::write(&staged, bytes).map_err(fail)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).map_err(fail)?;
    }

    // A leftover from a previous update. Its absence is the normal case
    // and its removal failing is not fatal, since the rename below
    // replaces it anyway on every platform hop runs on.
    let _ = std::fs::remove_file(&previous);
    std::fs::rename(target, &previous).map_err(fail)?;

    if let Err(error) = std::fs::rename(&staged, target) {
        // Put the working binary back rather than leaving no hop at all.
        let _ = std::fs::rename(&previous, target);
        let _ = std::fs::remove_file(&staged);
        return Err(fail(error));
    }
    Ok(())
}

/// Deletes the previous binary left behind by an update. Called on every
/// start; a no-op when there is nothing to clean up.
pub fn clean_previous() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("old"));
    }
}

/// Checks for a newer release and installs it. Returns the version
/// installed, or `None` if this build is already current.
#[cfg(not(target_os = "windows"))]
pub fn update(_check_only: bool) -> Result<Option<Version>, UpdateError> {
    Err(UpdateError::NotOnMacos)
}

/// Checks for a newer release and installs it. Returns the version
/// installed, or `None` if this build is already current.
#[cfg(target_os = "windows")]
pub fn update(check_only: bool) -> Result<Option<Version>, UpdateError> {
    let current = Version::current().unwrap_or(Version {
        major: 0,
        minor: 0,
        patch: 0,
    });
    let release = fetch_latest()?;

    if !is_upgrade(current, release.version) {
        return Ok(None);
    }
    if check_only {
        return Ok(Some(release.version));
    }

    let bytes = download(&release.download_url)?;
    let target = std::env::current_exe().map_err(UpdateError::NoInstallPath)?;
    replace_executable(&target, &bytes)?;
    Ok(Some(release.version))
}

/// Checks for and installs an update, then restarts hop into it.
///
/// Called at the start of `hop run` on Windows. Never returns on a
/// successful update: the new binary is started with the same arguments
/// and this process exits, so the user's only step is starting hop, which
/// they were doing anyway.
///
/// Every failure here is logged and swallowed. An update is a
/// convenience, and hop failing to start because GitHub was unreachable
/// would be a far worse bug than running a version that is one fix
/// behind.
pub fn update_and_restart() -> Option<std::convert::Infallible> {
    match update(false) {
        Ok(None) => None,
        Ok(Some(version)) => {
            tracing::info!(%version, "updated hop; restarting into the new build");
            let exe = std::env::current_exe().ok()?;
            let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
            match std::process::Command::new(exe).args(args).spawn() {
                Ok(_) => std::process::exit(0),
                Err(error) => {
                    // The new binary is in place; it will simply be used
                    // the next time the user starts hop themselves.
                    tracing::warn!(%error, "updated, but could not restart; start hop again to use it");
                    None
                }
            }
        }
        Err(error) => {
            tracing::warn!(%error, "update check failed; carrying on with this build");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_tag_with_or_without_its_v() {
        let want = Version {
            major: 1,
            minor: 2,
            patch: 3,
        };
        assert_eq!(Version::parse("1.2.3"), Some(want));
        assert_eq!(Version::parse("v1.2.3"), Some(want));
    }

    #[test]
    fn refuses_a_tag_it_cannot_read_exactly() {
        // A tag hop cannot parse is a tag it must not act on: the cost of
        // guessing wrong is overwriting a working binary.
        for bad in ["1.2", "1.2.3.4", "v1.2.3-rc1", "latest", "", "v", "1.2.x"] {
            assert_eq!(Version::parse(bad), None, "{bad} should not parse");
        }
    }

    #[test]
    fn versions_order_by_component_not_by_string() {
        // The string comparison trap: "0.10.0" sorts before "0.9.0".
        let ten = Version::parse("0.10.0").unwrap();
        let nine = Version::parse("0.9.0").unwrap();
        assert!(ten > nine);
        assert!(is_upgrade(nine, ten));
        assert!(!is_upgrade(ten, nine));
    }

    #[test]
    fn the_same_version_is_not_an_upgrade() {
        let v = Version::parse("0.1.0").unwrap();
        assert!(!is_upgrade(v, v));
    }

    #[test]
    fn an_older_release_never_drags_the_binary_backwards() {
        // A release pulled and re-cut lower, or a local build ahead of
        // what is published. Downgrading silently would be worse than
        // doing nothing.
        let installed = Version::parse("0.4.0").unwrap();
        let published = Version::parse("0.3.9").unwrap();
        assert!(!is_upgrade(installed, published));
    }

    #[test]
    fn this_build_knows_its_own_version() {
        assert!(
            Version::current().is_some(),
            "the crate version must stay parseable, since the updater compares against it"
        );
    }

    /// The shape GitHub actually returns, trimmed to the fields hop reads.
    const RELEASE_JSON: &str = r#"{
        "tag_name": "v0.2.0",
        "name": "v0.2.0",
        "assets": [
            {
                "name": "hop",
                "browser_download_url": "https://example.invalid/hop"
            },
            {
                "name": "hop.exe",
                "browser_download_url": "https://example.invalid/hop.exe"
            }
        ]
    }"#;

    #[test]
    fn picks_this_platforms_asset_out_of_the_release() {
        let release = release_from_json(RELEASE_JSON, "hop.exe").expect("should parse");
        assert_eq!(release.version, Version::parse("0.2.0").unwrap());
        assert_eq!(release.download_url, "https://example.invalid/hop.exe");

        // The two assets differ only by name, so picking the wrong one is
        // a real possibility worth pinning: a Mac binary on a PC.
        let mac = release_from_json(RELEASE_JSON, "hop").expect("should parse");
        assert_eq!(mac.download_url, "https://example.invalid/hop");
    }

    #[test]
    fn a_release_without_this_platforms_build_is_an_error_not_a_guess() {
        let json = r#"{"tag_name": "v0.2.0", "assets": [{"name": "hop", "browser_download_url": "https://example.invalid/hop"}]}"#;
        assert!(matches!(
            release_from_json(json, "hop.exe"),
            Err(UpdateError::NoAssetForPlatform(_))
        ));
    }

    #[test]
    fn a_reply_that_is_not_a_release_is_rejected() {
        for bad in [
            "not json at all",
            "{}",
            r#"{"tag_name": "nightly", "assets": []}"#,
            r#"{"tag_name": "v0.2.0"}"#,
        ] {
            assert!(
                release_from_json(bad, "hop.exe").is_err(),
                "{bad} should be refused"
            );
        }
    }

    #[test]
    fn splits_a_url_into_the_host_and_path_the_platform_call_wants() {
        assert_eq!(
            split_https_url("https://api.github.com/repos/a/b/releases/latest"),
            Some(("api.github.com".into(), "/repos/a/b/releases/latest".into()))
        );
        // A release asset URL, which is the other shape this ever sees.
        assert_eq!(
            split_https_url("https://github.com/a/b/releases/download/v1.0.0/hop.exe"),
            Some((
                "github.com".into(),
                "/a/b/releases/download/v1.0.0/hop.exe".into()
            ))
        );
        assert_eq!(
            split_https_url("https://example.invalid"),
            Some(("example.invalid".into(), "/".into()))
        );
    }

    #[test]
    fn refuses_a_url_that_is_not_plain_https() {
        // hop is about to overwrite its own executable with whatever
        // comes back, so anything but https is refused rather than
        // upgraded or tolerated.
        for bad in [
            "http://example.invalid/hop.exe",
            "ftp://example.invalid/hop.exe",
            "https://",
            "/repos/a/b",
            "https://user@evil.invalid/hop.exe",
        ] {
            assert_eq!(split_https_url(bad), None, "{bad} should be refused");
        }
    }

    #[test]
    fn an_error_page_is_never_mistaken_for_a_build() {
        // The realistic failure is GitHub serving HTML where a binary was
        // expected. Writing that over hop.exe leaves nothing that runs.
        assert!(!is_plausible_build(0));
        assert!(!is_plausible_build(4_096));
        assert!(!is_plausible_build(499_999));
        assert!(is_plausible_build(2_400_000));
        assert!(!is_plausible_build(u64::MAX));
    }

    #[test]
    fn replacing_a_binary_keeps_the_old_one_alongside_it() {
        // The rename dance is what lets a RUNNING hop.exe be replaced on
        // Windows, which will not allow it to be overwritten in place.
        let dir = std::env::temp_dir().join(format!("hop-update-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let target = dir.join("hop-test-binary");
        std::fs::write(&target, b"the currently running build").expect("seed");

        replace_executable(&target, b"the freshly downloaded build").expect("replace");

        assert_eq!(
            std::fs::read(&target).expect("read new"),
            b"the freshly downloaded build"
        );
        assert_eq!(
            std::fs::read(target.with_extension("old")).expect("read old"),
            b"the currently running build",
            "the running binary must survive under .old, since the process is still executing it"
        );
        assert!(
            !target.with_extension("new").exists(),
            "the staging file must not be left behind"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
