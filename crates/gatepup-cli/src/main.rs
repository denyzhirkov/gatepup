use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use gatepup_core::{load_validated, print_config, serve_from_file, CoreError};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "gatepup",
    version,
    about = "Tiny watchdog for your web traffic."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the proxy with the given config.
    Run {
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
    },
    /// Validate a config file and report any problems.
    Validate {
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
    },
    /// Validate and print the effective config as JSON.
    PrintConfig {
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
    },
}

fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();

    match cli.command {
        Command::Run { config } => run(config),
        Command::Validate { config } => match load_validated(&config) {
            Ok(cfg) => {
                println!("Config is valid: {} ({}).", config.display(), cfg.app.name);
                ExitCode::SUCCESS
            }
            Err(err) => report(err),
        },
        Command::PrintConfig { config } => match print_config(&config) {
            Ok(rendered) => {
                println!("{rendered}");
                ExitCode::SUCCESS
            }
            Err(err) => report(err),
        },
    }
}

/// Build a multi-threaded Tokio runtime and serve the proxy until shutdown.
fn run(config: PathBuf) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("error: failed to start runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(serve_from_file(&config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => report(err),
    }
}

/// Render a use-case failure for humans. Validation failures list every
/// problem; other failures print their message.
fn report(err: CoreError) -> ExitCode {
    if let Some(problems) = err.validation_errors() {
        eprintln!("Config is invalid ({} problem(s)):", problems.len());
        for problem in problems {
            eprintln!("  - {problem}");
        }
    } else {
        eprintln!("error: {err}");
    }
    ExitCode::FAILURE
}

fn init_tracing() {
    let filter = EnvFilter::try_from_env("GATEPUP_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
