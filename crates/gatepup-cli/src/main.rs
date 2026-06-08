use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use gatepup_core::{print_config, resolve_validated, serve, CoreError};
use gatepup_observability::init_logging;

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
    /// Run the proxy. Config comes from --config or the environment.
    Run {
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
    },
    /// Validate the config (file or environment) and report any problems.
    Validate {
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
    },
    /// Validate and print the effective config as JSON.
    PrintConfig {
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Command::Run { config } => run(config),
        Command::Validate { config } => match resolve_validated(config.as_deref()) {
            Ok((cfg, source)) => {
                println!("Config is valid ({source:?}): {}.", cfg.app.name);
                ExitCode::SUCCESS
            }
            Err(err) => report(err),
        },
        Command::PrintConfig { config } => match print_config(config.as_deref()) {
            Ok(rendered) => {
                println!("{rendered}");
                ExitCode::SUCCESS
            }
            Err(err) => report(err),
        },
    }
}

/// Load and validate the config, install JSON logging, then serve until Ctrl-C.
fn run(config_path: Option<PathBuf>) -> ExitCode {
    let config = match resolve_validated(config_path.as_deref()) {
        Ok((cfg, _source)) => cfg,
        Err(err) => return report(err),
    };
    init_logging(&config.app.log_level);

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

    match runtime.block_on(serve(config, config_path)) {
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
