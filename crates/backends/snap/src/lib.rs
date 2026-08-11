//! Snap backend for snowcone.
//!
//! snapd's `refresh` verb is our UPGRADE operation, and snap has no
//! user-triggered index refresh at all - so REFRESH is not declared and
//! `snap refresh --list` serves as the outdated listing instead. Mutations
//! need root from a classic shell, so snowcone prefixes the elevation
//! helper; snap never asks for confirmation, so there is no yes-flag for
//! `assume_yes` to forward, and no verb has a simulate flag. Reads are
//! aligned-column output parsed under `LC_ALL=C`; `snap info` is YAML-ish
//! `key: value` text with an indented channel map.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "snap";
const PROGRAMS: &[&str] = &["snap"];

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
    /// Mutating invocation, in the user's locale (output is passed through).
    fn cmd(&self) -> Cmd {
        Cmd::new(&self.program)
    }

    /// Read invocation with a stable locale, so parsing survives i18n.
    fn query(&self) -> Cmd {
        Cmd::new(&self.program).env("LC_ALL", "C")
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

    /// Mutating command through the elevation helper; snap has no prompts,
    /// so there is nothing for `assume_yes` to pass.
    fn mutation(&self, subcommand: &str) -> Cmd {
        self.cmd().arg(subcommand).elevated(true)
    }

    fn no_dry_run(&self, operation: &str) -> Error {
        Error::Other(format!("{ID}: {operation} has no dry-run mode"))
    }

    /// Installed snaps from `snap list`.
    async fn installed(&self) -> Result<Vec<SnapPackage>> {
        let output = self
            .query()
            .arg("list")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_list(&output.stdout))
    }
}

/// Snap installs track channels, not versions; there is no version pin.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but snap installs track channels"
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
        "Snap"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Universal
    }

    fn database_id(&self) -> &'static str {
        "snapd"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
    }

    fn needs_elevation(&self, operation: Operation) -> bool {
        matches!(
            operation,
            Operation::Install | Operation::Remove | Operation::Upgrade
        )
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        let cmd = self
            .mutation("install")
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        let cmd = self
            .mutation("remove")
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.installed().await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query()
            .arg("info")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        let package = parse_info(&output.stdout).ok_or_else(|| Error::Parse {
            what: format!("{ID} info output"),
            detail: format!("no `name` field for `{name}`"),
        })?;
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("find")
            .arg(query)
            .capture(&self.elevator, None)
            .await?;
        // An empty result is an error to snapd, not to us: it complains
        // `error: no matching snaps for "<query>"` on stderr.
        if !output.success() && output.stderr.contains("no matching snaps for") {
            return Ok(Vec::new());
        }
        let output = output.require_success()?;
        Ok(boxed(parse_find(&output.stdout)))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        let cmd = self
            .mutation("refresh")
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["refresh", "--list"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        let mut outdated = parse_refresh_list(&output.stdout);
        if outdated.is_empty() {
            return Ok(Vec::new());
        }
        // `refresh --list` names only the update's version; the installed
        // listing fills in the current one and the tracked channel.
        let installed = self.installed().await?;
        for package in &mut outdated {
            if let Some(current) = installed.iter().find(|local| local.name == package.name) {
                package.version = current.version.clone();
                package.origin = current.origin.clone();
            }
        }
        Ok(boxed(outdated))
    }
}

fn boxed(packages: Vec<SnapPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `snap list`: aligned `Name Version Rev Tracking Publisher Notes`
/// columns under a header line; an empty install prints a "No snaps"
/// notice instead.
fn parse_list(stdout: &str) -> Vec<SnapPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            if line.starts_with("No snaps") {
                return None;
            }
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            if name == "Name" {
                return None;
            }
            let version = parts.next()?;
            let _rev = parts.next()?;
            let tracking = parts.next()?;
            Some(SnapPackage {
                name: name.to_string(),
                version: Some(version.to_string()),
                origin: Some(tracking.to_string()),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `snap find`: aligned `Name Version Publisher Notes Summary` columns;
/// the summary is prose, so it is everything past the fourth column.
fn parse_find(stdout: &str) -> Vec<SnapPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            if name == "Name" {
                return None;
            }
            let version = parts.next()?;
            let _publisher = parts.next()?;
            let _notes = parts.next()?;
            let summary = parts.collect::<Vec<_>>().join(" ");
            Some(SnapPackage {
                name: name.to_string(),
                version: Some(version.to_string()),
                description: (!summary.is_empty()).then_some(summary),
                state: InstallState::Available,
                ..Default::default()
            })
        })
        .collect()
}

/// `snap refresh --list`: aligned `Name Version Rev Publisher Notes`
/// columns where Version is the version an update would install; an
/// up-to-date system prints an "All snaps up to date." notice instead.
fn parse_refresh_list(stdout: &str) -> Vec<SnapPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            if line.starts_with("All snaps up to date") {
                return None;
            }
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            if name == "Name" {
                return None;
            }
            let version = parts.next()?;
            Some(SnapPackage {
                name: name.to_string(),
                latest_version: Some(version.to_string()),
                state: InstallState::Upgradable,
                ..Default::default()
            })
        })
        .collect()
}

/// `snap info`: top-level `key: value` lines with an indented `channels:`
/// map whose `^` and dash entries mean "no release of its own"; the
/// `installed:` line carries the local version, the tracked channel the
/// store's.
fn parse_info(stdout: &str) -> Option<SnapPackage> {
    let mut package = SnapPackage::default();
    let mut tracking: Option<String> = None;
    let mut installed: Option<String> = None;
    let mut channels: Vec<(String, String)> = Vec::new();
    let mut in_channels = false;
    for line in stdout.lines() {
        if line.starts_with(char::is_whitespace) {
            if in_channels
                && let Some((channel, value)) = line.trim().split_once(':')
                && let Some(version) = value.split_whitespace().next()
                && !matches!(version, "^" | "-" | "--" | "\u{2013}" | "\u{2014}")
            {
                channels.push((channel.trim().to_string(), version.to_string()));
            }
            continue;
        }
        in_channels = false;
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key {
            "name" => package.name = value.to_string(),
            "summary" => package.description = field(value),
            "store-url" => package.homepage = field(value),
            "license" => package.license = field(value),
            "tracking" => tracking = field(value),
            "installed" => installed = value.split_whitespace().next().map(str::to_string),
            "channels" => in_channels = true,
            _ => {}
        }
    }
    let channel_version = |channel: &str| {
        channels
            .iter()
            .find(|(name, _)| name == channel)
            .map(|(_, version)| version.clone())
    };
    let store = tracking
        .as_deref()
        .and_then(channel_version)
        .or_else(|| channel_version("latest/stable"))
        .or_else(|| channels.first().map(|(_, version)| version.clone()));
    match installed {
        Some(version) => {
            package.state = InstallState::Installed;
            if let Some(store) = store
                && store != version
            {
                package.latest_version = Some(store);
                package.state = InstallState::Upgradable;
            }
            package.version = Some(version);
            package.origin = tracking;
        }
        None => {
            package.state = InstallState::Available;
            package.version = store;
        }
    }
    (!package.name.is_empty()).then_some(package)
}

/// Present-and-meaningful field values; snapd prints `unset` for a
/// missing license.
fn field(value: &str) -> Option<String> {
    (!value.is_empty() && value != "unset").then(|| value.to_string())
}

/// A package as snap describes it.
#[derive(Debug, Default)]
pub struct SnapPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub origin: Option<String>,
    pub state: InstallState,
}

impl Package for SnapPackage {
    fn manager(&self) -> &str {
        ID
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    fn latest_version(&self) -> Option<&str> {
        self.latest_version.as_deref()
    }

    fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    fn homepage(&self) -> Option<&str> {
        self.homepage.as_deref()
    }

    fn license(&self) -> Option<&str> {
        self.license.as_deref()
    }

    fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_list_columns() {
        let stdout = "\
Name    Version      Rev    Tracking       Publisher   Notes
core22  20240111     1122   latest/stable  canonical\u{2713}  base
hello   2.10         42     latest/stable  canonical\u{2713}  -
";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "hello");
        assert_eq!(packages[1].version.as_deref(), Some("2.10"));
        assert_eq!(packages[1].origin.as_deref(), Some("latest/stable"));
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn empty_list_notice_parses_to_nothing() {
        let stdout = "No snaps are installed yet. Try 'snap install hello-world'.\n";
        assert!(parse_list(stdout).is_empty());
    }

    #[test]
    fn parses_find_columns_with_prose_summary() {
        let stdout = "\
Name         Version  Publisher   Notes  Summary
hello        2.10     canonical\u{2713}  -      GNU Hello, the \"hello world\" snap
hello-world  6.4      canonical\u{2713}  -      The 'hello-world' of snaps
";
        let packages = parse_find(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "hello");
        assert_eq!(
            packages[0].description.as_deref(),
            Some("GNU Hello, the \"hello world\" snap")
        );
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn parses_refresh_list_columns() {
        let stdout = "\
Name   Version  Rev  Publisher   Notes
hello  2.12     48   canonical\u{2713}  -
";
        let packages = parse_refresh_list(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].latest_version.as_deref(), Some("2.12"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
        assert!(parse_refresh_list("All snaps up to date.\n").is_empty());
    }

    #[test]
    fn parses_info_of_upgradable_snap() {
        let stdout = "\
name:      hello
summary:   GNU Hello, the \"hello world\" snap
publisher: Canonical\u{2713}
store-url: https://snapcraft.io/hello
license:   GPL-3.0
description: |
  GNU hello prints a friendly greeting.
snap-id:      mVyGrEwiqSi5PugCwyH7WgpoQLemtTd6
tracking:     latest/stable
refresh-date: today at 12:00 UTC
channels:
  latest/stable:    2.12 2023-10-05   (42) 98kB -
  latest/candidate: ^
  latest/beta:      ^
  latest/edge:      2.13 2024-01-10   (45) 99kB -
installed:          2.10                (38) 98kB -
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.name, "hello");
        assert_eq!(package.version.as_deref(), Some("2.10"));
        assert_eq!(package.latest_version.as_deref(), Some("2.12"));
        assert_eq!(package.state, InstallState::Upgradable);
        assert_eq!(package.origin.as_deref(), Some("latest/stable"));
        assert_eq!(
            package.homepage.as_deref(),
            Some("https://snapcraft.io/hello")
        );
        assert_eq!(package.license.as_deref(), Some("GPL-3.0"));
        assert_eq!(
            package.description.as_deref(),
            Some("GNU Hello, the \"hello world\" snap")
        );
    }

    #[test]
    fn parses_info_of_available_snap() {
        let stdout = "\
name:      hello
summary:   GNU Hello, the \"hello world\" snap
publisher: Canonical\u{2713}
license:   unset
channels:
  latest/stable:    2.12 2023-10-05   (42) 98kB -
  latest/edge:      2.13 2024-01-10   (45) 99kB -
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.version.as_deref(), Some("2.12"));
        assert_eq!(package.state, InstallState::Available);
        assert_eq!(package.license, None);
        assert_eq!(package.latest_version, None);
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("hello@2.10")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("hello")]).is_ok());
    }
}
