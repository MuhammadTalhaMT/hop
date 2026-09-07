//! `hop`'s command-line entry point: `keygen` and `run`, on top of the
//! configuration loading in `hop::config` and the platform/core crates
//! that do the actual work.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod run;
mod update;

#[cfg(target_os = "macos")]
mod menubar;

#[derive(Parser)]
#[command(
    name = "hop",
    version,
    about = "Keyboard and mouse sharing between macOS and Windows"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
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
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    init_logging();

    // Delete the binary the last update moved aside. Windows cannot
    // overwrite a running executable, so an update renames it instead and
    // the leftover is cleared here, once it is no longer running.
    update::clean_previous();

    let cli = Cli::parse();

    let result = match cli.command {
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
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
