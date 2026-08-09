use clap::{Parser, Subcommand};

/// One package manager to rule them all.
#[derive(Debug, Parser)]
#[command(name = "snow", version, about)]
pub struct Cli {
    /// Only use these backends (repeat or comma-separate: -m apt,flatpak).
    #[arg(short, long = "manager", global = true, value_delimiter = ',')]
    pub manager: Vec<String>,

    /// Machine-readable JSON on stdout.
    #[arg(long, global = true)]
    pub json: bool,

    /// Assume "yes" for backend prompts.
    #[arg(short = 'y', long = "yes", global = true)]
    pub assume_yes: bool,

    /// Show what would happen without changing anything (best effort).
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// With no command, `snow` opens the TUI.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Search available packages across all detected managers.
    Search { query: String },
    /// Show detailed metadata for a package.
    Info { package: String },
    /// Install packages (NAME or NAME@VERSION).
    Install {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Remove installed packages.
    Remove {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Upgrade specific packages, or everything when none are given.
    Upgrade { packages: Vec<String> },
    /// Refresh package metadata/indexes.
    Refresh,
    /// List installed packages.
    List {
        /// Only packages with a newer version available.
        #[arg(long)]
        outdated: bool,
    },
    /// Show detected package managers and their capabilities.
    Managers,
    /// Open the interactive TUI (default when no command is given).
    Tui,
}
