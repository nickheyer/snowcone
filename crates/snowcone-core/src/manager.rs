//! The package manager interface.
//!
//! The four required operations are the universal subset — every manager in
//! the README, down to `dpkg`, `rpm`, and Slackware's pkgtools, can perform
//! them. The capability-gated operations default to
//! [`Error::Unsupported`](crate::Error::Unsupported) so low-level backends
//! simply don't override them.

use serde::Serialize;
use std::fmt;

use crate::capability::{Capabilities, Operation};
use crate::error::{Error, Result};
use crate::package::{Package, PackageRequest};
use crate::progress::{EventSender, ProgressEvent};

/// Rough grouping of managers, mostly for presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManagerKind {
    /// Distro-native (apt, dnf, pacman, …).
    System,
    /// Distro-agnostic app/package delivery (flatpak, snap, brew, nix, …).
    Universal,
    /// Language ecosystem (cargo, pip, npm, …).
    Language,
    Other,
}

impl fmt::Display for ManagerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ManagerKind::System => "system",
            ManagerKind::Universal => "universal",
            ManagerKind::Language => "language",
            ManagerKind::Other => "other",
        })
    }
}

/// Options and plumbing shared by every mutating operation.
#[derive(Clone, Debug, Default)]
pub struct OpContext {
    /// Answer backend prompts with "yes".
    pub assume_yes: bool,
    /// Best effort: backends that can, preview instead of acting.
    pub dry_run: bool,
    /// Where to stream progress; `None` means the backend may talk to the
    /// terminal directly (CLI passthrough).
    pub events: Option<EventSender>,
}

impl OpContext {
    pub fn with_events(mut self, events: EventSender) -> Self {
        self.events = Some(events);
        self
    }

    pub fn emit(&self, event: ProgressEvent) {
        if let Some(events) = &self.events {
            let _ = events.send(event);
        }
    }
}

/// The broadest common subset of package manager functionality, as one
/// trait. Backend crates implement this once per manager.
#[async_trait::async_trait]
pub trait PackageManager: Send + Sync {
    /// Stable unique id, used for `--manager` and package attribution
    /// (e.g. `"apt"`, `"flatpak"`, `"cargo"`).
    fn id(&self) -> &'static str;

    /// Human-facing name (e.g. `"APT"`, `"Flatpak"`).
    fn display_name(&self) -> &'static str;

    fn kind(&self) -> ManagerKind;

    /// Identifier of the package database/state this manager mutates
    /// (`"dpkg"`, `"alpm"`, `"python"`, …). Managers sharing a database are
    /// grouped, and each operation is routed to one elected member — see
    /// [`crate::election`].
    fn database_id(&self) -> &'static str;

    fn capabilities(&self) -> Capabilities;

    fn supports(&self, operation: Operation) -> bool {
        self.capabilities().contains(operation.capability())
    }

    /// Whether this operation must run through an elevation helper on this
    /// host (system managers say yes for mutations, user-scoped ones never
    /// do).
    fn needs_elevation(&self, operation: Operation) -> bool {
        let _ = operation;
        false
    }

    // ---- Universal operations (every backend implements these) ----

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()>;

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()>;

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>>;

    async fn info(&self, name: &str) -> Result<Box<dyn Package>>;

    // ---- Capability-gated operations (default: unsupported) ----

    /// Search packages available to this manager.
    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let _ = query;
        Err(self.unsupported(Operation::Search))
    }

    /// Refresh remote metadata/indexes (`apt update`, `pacman -Sy`, …).
    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        let _ = ctx;
        Err(self.unsupported(Operation::Refresh))
    }

    /// Upgrade the given packages, or everything this manager tracks when
    /// `packages` is empty.
    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let _ = (packages, ctx);
        Err(self.unsupported(Operation::Upgrade))
    }

    /// Installed packages with a newer version available.
    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        Err(self.unsupported(Operation::ListOutdated))
    }

    /// Helper for default implementations and backends alike.
    fn unsupported(&self, operation: Operation) -> Error {
        Error::Unsupported {
            manager: self.id().to_string(),
            operation,
        }
    }
}
