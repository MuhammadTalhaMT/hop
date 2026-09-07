// Built as a Windows GUI application, so double clicking hop, or starting
// it from the tray, never flashes up a console window. That is the whole
// point of the tray existing. `hop` reattaches to the parent's console at
// startup (see `hop_platform::windows::console`) so the command line half
// still prints normally.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! `hop`'s command-line entry point: `keygen` and `run`, on top of the
//! configuration loading in `hop::config` and the platform/core crates
//! that do the actual work.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod run;
mod update;

#[cfg(target_os = "windows")]
mod tray;

#[cfg(target_os = "macos")]
mod menubar;

#[derive(Parser)]
#[command(
    name = "hop",
    version,
    about = "Keyboard and mouse sharing between macOS and Windows"
)]
struct Cli {
    /// Omitted entirely when hop is double clicked, which is the case the
    /// tray exists for: with no subcommand, Windows shows the tray icon
    /// rather than printing a usage error into a console that is not
    /// there.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a shared key and write it to disk, base64 encoded.
    Keygen {
        /// Config file to read the key's destination from, if --out is
        /// not given.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Where to write the key. Overrides the config's
        /// [security] key_file.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Overwrite an existing key file.
        #[arg(long)]
        force: bool,
    },
    /// Update hop in place from the newest GitHub release.
    ///
    /// Windows only: on the Mac, Accessibility permission is tied to the
    /// signing certificate, so replacing this binary with an unsigned CI
    /// build would silently break input capture. Use ./update-mac.sh.
    Update {
        /// Report whether an update exists without installing it.
        #[arg(long)]
        check: bool,
    },
    /// Load a config file and run hop as the role it specifies.
    Run {
        /// Path to the TOML config file.
        #[arg(long)]
        config: PathBuf,
        /// Skip the update check that normally runs at startup on
        /// Windows.
        #[arg(long)]
        no_update: bool,
    },
    /// Show a menu bar item for starting and stopping hop (macOS only).
    #[cfg(target_os = "macos")]
    Menubar {
        /// Path to the TOML config file hop will be started with.
        #[arg(long)]
        config: PathBuf,
    },
    /// Show a notification area icon for starting and stopping hop
    /// (Windows only). This is what running hop without a console looks
    /// like, and it is what double clicking hop.exe does.
    #[cfg(target_os = "windows")]
    Tray {
        /// Path to the TOML config file hop will be started with.
        /// Defaults to %APPDATA%\hop\config.toml.
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

/// Where hop looks for its config when nobody says.
///
/// Only used by the tray, and only because the tray is the one entry
/// point that can be reached with no arguments at all, by double
/// clicking. Every other command still requires --config, so nothing
/// silently reads a file the user did not name.
#[cfg(target_os = "windows")]
fn default_config_path() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(base).join("hop").join("config.toml")
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    init_logging();

    // Give this process the console it was launched from, if any. hop is
    // a GUI application on Windows so that the tray never flashes a
    // console window; without this, running it from a command prompt
    // would print into nowhere.
    #[cfg(target_os = "windows")]
    hop_platform::windows::console::attach_parent();

    // Delete the binary the last update moved aside. Windows cannot
    // overwrite a running executable, so an update renames it instead and
    // the leftover is cleared here, once it is no longer running.
    update::clean_previous();

    let cli = Cli::parse();

    let command = match cli.command {
        Some(command) => command,
        // No subcommand at all. On Windows that means hop was double
        // clicked, so show the tray rather than a usage error nobody can
        // see; anywhere else, print the usage as normal.
        #[cfg(target_os = "windows")]
        None => Command::Tray { config: None },
        #[cfg(not(target_os = "windows"))]
        None => {
            use clap::CommandFactory;
            Cli::command().print_help().ok();
            return ExitCode::FAILURE;
        }
    };

    let result = match command {
        Command::Keygen { config, out, force } => {
            run::keygen(config.as_deref(), out.as_deref(), force)
        }
        Command::Update { check } => match update::update(check) {
            Ok(Some(version)) if check => {
                println!("hop {version} is available; run `hop update` to install it");
                Ok(())
            }
            Ok(Some(version)) => {
                println!("updated to hop {version}; restart hop to use it");
                Ok(())
            }
            Ok(None) => {
                println!(
                    "hop {} is already the newest release",
                    env!("CARGO_PKG_VERSION")
                );
                Ok(())
            }
            Err(error) => Err(error.into()),
        },
        Command::Run { config, no_update } => {
            // Update before doing anything else, so the user's only step
            // is starting hop. Windows only, and never fatal: see
            // `update::update_and_restart`.
            if cfg!(target_os = "windows") && !no_update {
                update::update_and_restart();
            }
            run::run(&config).await
        }
        #[cfg(target_os = "macos")]
        Command::Menubar { config } => menubar::run(config),
        #[cfg(target_os = "windows")]
        Command::Tray { config } => {
            tray::run(config.unwrap_or_else(default_config_path)).map_err(run::RunError::Tray)
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
