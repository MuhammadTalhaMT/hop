//! `hop`'s command-line entry point: `keygen` and `run`, on top of the
//! configuration loading in `hop::config` and the platform/core crates
//! that do the actual work.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod run;

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
    /// Load a config file and run hop as the role it specifies.
    Run {
        /// Path to the TOML config file.
        #[arg(long)]
        config: PathBuf,
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

    let cli = Cli::parse();

    let result = match cli.command {
        Command::Keygen { config, out, force } => {
            run::keygen(config.as_deref(), out.as_deref(), force)
        }
        Command::Run { config } => run::run(&config).await,
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
