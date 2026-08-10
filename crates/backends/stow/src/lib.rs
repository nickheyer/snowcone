//! GNU Stow backend for snowcone.
//!
//! Stow is a symlink-farm manager, not a downloader: a "package" is a
//! directory under a stow root, install means linking its tree into the
//! root's parent (`stow`), remove means unlinking it (`stow -D`). This
//! backend manages the conventional system farm at `/usr/local/stow` - the
//! path is fixed because backends carry no configuration channel - and every
//! operation errors plainly when that directory is missing. Stow keeps no
//! database and cannot report what is currently linked, so the honest
//! installed listing is the stow root's subdirectories, and info describes a
//! package directory's existence and top-level contents. `-n` (--simulate)
//! is a native dry-run for both mutations; stow never prompts, so
//! `assume_yes` has nothing to do. `/usr/local` is root-owned, so mutations
//! run through the elevation helper.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "stow";
const PROGRAMS: &[&str] = &["stow"];
const STOW_ROOT: &str = "/usr/local/stow";

pub fn factory() -> Box<dyn BackendFactory> {
    Box::new(Factory)
}

struct Factory;

impl BackendFactory for Factory {
    fn id(&self) -> &'static str {
        ID
    }

    fn detect(&self, _host: &HostInfo) -> Detection {
        match PROGRAMS.iter().find_map(|program| find_program(program)) {
            Some(program) => Detection::Available { program },
            None => Detection::Unavailable {
                reason: format!("`{}` not found on PATH", PROGRAMS[0]),
            },
        }
    }

    fn create(&self, host: &HostInfo) -> Result<Box<dyn PackageManager>> {
        let program = PROGRAMS
            .iter()
            .find_map(|program| find_program(program))
            .ok_or_else(|| Error::Unavailable(ID.to_string()))?;
        Ok(Box::new(Manager {
            program,
            elevator: Elevator::detect(host),
        }))
    }
}

struct Manager {
    program: PathBuf,
    elevator: Elevator,
}

impl Manager {
    fn cmd(&self) -> Cmd {
        Cmd::new(&self.program)
    }

    /// CLI passthrough when no event consumer is attached, captured and
    /// streamed otherwise.
    async fn run(&self, cmd: Cmd, ctx: &OpContext) -> Result<()> {
        let output = match &ctx.events {
            Some(events) => cmd.capture(&self.elevator, Some(events)).await?,
            None => cmd.run_interactive(&self.elevator).await?,
        };
        output.require_success()?;
        Ok(())
    }

    /// Mutating stow invocation against the fixed root, verbose so both real
    /// runs and `-n` previews narrate their link operations (stow is silent
    /// otherwise, which would make dry-run output useless).
    fn mutation(&self, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().args(["-d", STOW_ROOT, "-v"]).elevated(true);
        if ctx.dry_run {
            cmd = cmd.arg("-n");
        }
        cmd
    }
}

/// The fixed stow root, checked up front so failures name the missing
/// directory instead of surfacing stow's own error.
fn stow_root() -> Result<PathBuf> {
    let root = PathBuf::from(STOW_ROOT);
    if root.is_dir() {
        Ok(root)
    } else {
        Err(Error::Other(format!(
            "{ID}: stow root `{STOW_ROOT}` does not exist; this backend manages that fixed system farm and needs it created first"
        )))
    }
}

/// Package names are single directory names under the root; separators or
/// traversal components could escape the farm.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(Error::Other(format!(
            "{ID}: `{name}` is not a valid stow package name"
        )));
    }
    Ok(())
}

/// Stow packages are plain directories; there is no version to choose.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but stow packages are unversioned directories"
        ))),
        None => Ok(()),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "GNU Stow"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Universal
    }

    fn database_id(&self) -> &'static str {
        "stow"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
    }

    fn needs_elevation(&self, operation: Operation) -> bool {
        matches!(operation, Operation::Install | Operation::Remove)
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        stow_root()?;
        for package in packages {
            validate_name(&package.name)?;
        }
        let cmd = self
            .mutation(ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        stow_root()?;
        for package in packages {
            validate_name(&package.name)?;
        }
        let cmd = self
            .mutation(ctx)
            .arg("-D")
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let root = stow_root()?;
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            if entry.path().is_dir() {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        Ok(packages_from_entries(names)
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        if validate_name(name).is_err() {
            return Err(Error::NotFound(name.to_string()));
        }
        let dir = stow_root()?.join(name);
        if !dir.is_dir() {
            return Err(Error::NotFound(name.to_string()));
        }
        let mut entries: Vec<String> = std::fs::read_dir(&dir)?
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        Ok(Box::new(StowPackage {
            name: name.to_string(),
            description: Some(summarize_entries(&entries)),
            state: InstallState::Installed,
        }))
    }
}

/// Directory entries → packages: hidden entries are not packages, and the
/// listing is sorted for stable output. Presence under the root is the only
/// state stow can attest.
fn packages_from_entries(mut names: Vec<String>) -> Vec<StowPackage> {
    names.retain(|name| !name.starts_with('.'));
    names.sort();
    names
        .into_iter()
        .map(|name| StowPackage {
            name,
            description: None,
            state: InstallState::Installed,
        })
        .collect()
}

/// One-line contents summary for info: entry count plus the first few
/// top-level names.
fn summarize_entries(entries: &[String]) -> String {
    const SHOWN: usize = 10;
    if entries.is_empty() {
        return "empty package directory".to_string();
    }
    let mut shown = entries
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if entries.len() > SHOWN {
        shown.push_str(", …");
    }
    let noun = if entries.len() == 1 { "entry" } else { "entries" };
    format!("{} top-level {noun}: {shown}", entries.len())
}

/// A stow package: a directory under the stow root. Stow yields no version
/// metadata - the directory name is the whole identity.
#[derive(Debug)]
pub struct StowPackage {
    pub name: String,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for StowPackage {
    fn manager(&self) -> &str {
        ID
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> Option<&str> {
        None
    }

    fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn listing_filters_hidden_entries_and_sorts() {
        let packages = packages_from_entries(names(&["zsh", ".git", "emacs", ".stow"]));
        let listed: Vec<&str> = packages.iter().map(|package| package.name.as_str()).collect();
        assert_eq!(listed, vec!["emacs", "zsh"]);
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn summarizes_entry_counts() {
        assert_eq!(summarize_entries(&[]), "empty package directory");
        assert_eq!(
            summarize_entries(&names(&["bin"])),
            "1 top-level entry: bin"
        );
        assert_eq!(
            summarize_entries(&names(&["bin", "lib", "share"])),
            "3 top-level entries: bin, lib, share"
        );
    }

    #[test]
    fn summarizes_long_listings_with_an_ellipsis() {
        let entries = names(&[
            "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l",
        ]);
        let summary = summarize_entries(&entries);
        assert!(summary.starts_with("12 top-level entries: a, b,"));
        assert!(summary.ends_with(", …"));
        assert!(!summary.contains('k'));
    }

    #[test]
    fn validates_package_names() {
        assert!(validate_name("emacs").is_ok());
        assert!(validate_name("emacs-29.1").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name(".").is_err());
        assert!(validate_name("..").is_err());
        assert!(validate_name("../etc").is_err());
        assert!(validate_name("a/b").is_err());
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("emacs@29.1")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("emacs")]).is_ok());
    }
}
