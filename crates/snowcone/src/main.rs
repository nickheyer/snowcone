mod cli;
mod commands;
mod config;
mod output;
mod picker;
mod relevance;
mod tui;

use clap::Parser;
use snowcone_core::{HostInfo, OpContext, Registry};
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Rust ignores SIGPIPE by default, which turns `snow … | head` into a
    // panic when the pipe closes. Restore the Unix default: exit quietly.
    // (No SIGPIPE concept on Windows.)
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::parse();
    let use_tui = matches!(&cli.command, None | Some(Command::Tui));

    if !use_tui {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
            )
            .with_writer(std::io::stderr)
            .init();
    }

    let host = HostInfo::detect();
    let registry = build_registry();

    if use_tui {
        return tui::run(host, registry).await;
    }

    let runner = commands::Runner {
        host,
        registry,
        filter: cli.manager,
        json: cli.json,
        op_ctx: OpContext {
            assume_yes: cli.assume_yes,
            dry_run: cli.dry_run,
            events: None,
        },
    };

    match cli.command.expect("non-TUI path always has a command") {
        Command::Search { query } => runner.search(&query).await,
        Command::Info { package } => runner.info(&package).await,
        Command::Install { packages } => runner.install(&packages).await,
        Command::Remove { packages } => runner.remove(&packages).await,
        Command::Upgrade { packages } => runner.upgrade(&packages).await,
        Command::Refresh => runner.refresh().await,
        Command::List { outdated } => runner.list(outdated).await,
        Command::Managers => runner.managers(),
        Command::Tui => unreachable!("handled above"),
    }
}

/// Every backend crate is wired in through the `snowcone-backends` umbrella
/// crate - one registration point, nowhere else.
fn build_registry() -> Registry {
    let mut registry = Registry::new();
    snowcone_backends::register_all(&mut registry);
    registry
}
