//! PEAR / PECL backend for snowcone.
//!
//! Drives the classic channel-based `pear` installer. Its output is
//! human-shaped tables, so every read runs under `LC_ALL=C` and the
//! parsers only accept rows carrying a version-shaped column. pear has no
//! yes-flag - it rarely prompts, though PECL-style extension builds may
//! ask configure questions interactively - and `--pretend` gives install
//! and upgrade a native dry run. The install tree is often root-owned on
//! system PHP setups, but the approved wiring keeps mutations unelevated.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "pear";
const PROGRAMS: &[&str] = &["pear", "pecl"];

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
                reason: format!("none of {PROGRAMS:?} found on PATH"),
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

    /// Shared shape for install/upgrade: `--pretend` is their native dry
    /// run; pear has no yes-flag, so `assume_yes` adds nothing.
    fn mutation(&self, verb: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(verb);
        if ctx.dry_run {
            cmd = cmd.arg("--pretend");
        }
        cmd
    }
}

/// pear pins are `Package-1.2.3`, but this backend does not declare
/// pin-version, so versioned requests are refused outright.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but this backend only installs the latest release"
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
        "PEAR / PECL"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "pear"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        let cmd = self
            .mutation("install", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: uninstall has no dry-run mode")));
        }
        let cmd = self
            .cmd()
            .arg("uninstall")
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["list", "--allchannels"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_list(&output.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // remote-info sees installed and available packages alike and
        // reports the installed version next to the latest; the local
        // `info` table covers offline hosts and unregistered channels.
        let remote = self
            .query()
            .arg("remote-info")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if remote.success()
            && let Some(package) = parse_remote_info(&remote.stdout)
        {
            return Ok(Box::new(package));
        }
        let local = self
            .query()
            .arg("info")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !local.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        parse_info(&local.stdout)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("search")
            .arg(query)
            .capture(&self.elevator, None)
            .await?;
        // "no packages found" is an error to pear but an empty result to
        // us; anything else non-zero (network, channel trouble) stays an
        // error.
        if output.stdout.contains("no packages found")
            || output.stderr.contains("no packages found")
        {
            return Ok(Vec::new());
        }
        let output = output.require_success()?;
        Ok(boxed(parse_search(&output.stdout)))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        // `pear upgrade` with no arguments upgrades every installed
        // package (upgrade-all is deprecated in its favor).
        let cmd = self
            .mutation("upgrade", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("list-upgrades")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_upgrades(&output.stdout)))
    }
}

fn boxed(packages: Vec<PearPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// Digits-and-dots version test that keeps `1.10.13` but drops sizes like
/// `21kB` and channel hosts like `pear.php.net`.
fn is_version(token: &str) -> bool {
    token.starts_with(|c: char| c.is_ascii_digit())
        && token.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// `pear list`: bordered per-channel tables; a data row is any line whose
/// second column starts with a digit (`Name  1.2.3  stable`), which skips
/// headers, rules, and channel banners.
fn parse_list(stdout: &str) -> Vec<PearPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            let version = parts.next()?;
            version
                .starts_with(|c: char| c.is_ascii_digit())
                .then(|| PearPackage {
                    name: name.to_string(),
                    version: Some(version.to_string()),
                    state: InstallState::Installed,
                    ..Default::default()
                })
        })
        .collect()
}

/// `pear search`: `Name version (stability) [local] summary…` rows under a
/// `Matched packages` banner; an installed hit carries its local version
/// as an extra digit-leading column before the summary.
fn parse_search(stdout: &str) -> Vec<PearPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            if line.starts_with(char::is_whitespace) {
                return None;
            }
            let mut parts = line.split_whitespace().peekable();
            let name = parts.next()?;
            let version = parts.next()?;
            if !version.starts_with(|c: char| c.is_ascii_digit()) {
                return None;
            }
            if let Some(next) = parts.peek()
                && next.starts_with('(')
            {
                parts.next();
            }
            let mut local = None;
            if let Some(next) = parts.peek()
                && next.starts_with(|c: char| c.is_ascii_digit())
            {
                local = parts.next().map(str::to_string);
            }
            let description = parts.collect::<Vec<_>>().join(" ");
            let mut package = PearPackage {
                name: name.to_string(),
                version: Some(version.to_string()),
                description: (!description.is_empty()).then_some(description),
                state: InstallState::Available,
                ..Default::default()
            };
            if let Some(local) = local {
                if local == version {
                    package.state = InstallState::Installed;
                } else {
                    package.state = InstallState::Upgradable;
                    package.latest_version = Some(version.to_string());
                    package.version = Some(local);
                }
            }
            Some(package)
        })
        .collect()
}

/// `pear remote-info`: a `Key value` table (`Latest`, `Installed`,
/// `Package`, `License`, `Summary`, …) where `Installed` reads `- no -`
/// when the package is absent locally.
fn parse_remote_info(stdout: &str) -> Option<PearPackage> {
    let mut package = PearPackage {
        state: InstallState::Available,
        ..Default::default()
    };
    let mut latest = None;
    let mut installed = None;
    for line in stdout.lines() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let (Some(key), Some(value)) = (parts.next(), parts.next().map(str::trim)) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        match key {
            "Package" => package.name = value.to_string(),
            "Latest" => latest = value.split_whitespace().next().map(str::to_string),
            "Installed" => {
                installed = value
                    .starts_with(|c: char| c.is_ascii_digit())
                    .then(|| value.split_whitespace().next().unwrap_or(value).to_string());
            }
            "License" => package.license = Some(value.to_string()),
            "Summary" => package.description = Some(value.to_string()),
            _ => {}
        }
    }
    match (installed, latest) {
        (Some(installed), Some(latest)) if installed != latest => {
            package.state = InstallState::Upgradable;
            package.version = Some(installed);
            package.latest_version = Some(latest);
        }
        (Some(installed), _) => {
            package.state = InstallState::Installed;
            package.version = Some(installed);
        }
        (None, latest) => package.version = latest,
    }
    (!package.name.is_empty()).then_some(package)
}

/// `pear info`: an `About …` banner then a fixed-width `Key   Value` table
/// whose keys can contain spaces (`Release Version`), so only known keys
/// are matched, by prefix.
fn parse_info(stdout: &str) -> Option<PearPackage> {
    let mut package = PearPackage {
        state: InstallState::Installed,
        ..Default::default()
    };
    for line in stdout.lines() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        if let Some(value) = field(line, "Name") {
            package.name = value.to_string();
        } else if let Some(value) = field(line, "Channel") {
            package.origin = Some(value.to_string());
        } else if let Some(value) = field(line, "Summary") {
            package.description = Some(value.to_string());
        } else if let Some(value) = field(line, "Release Version") {
            package.version = value.split_whitespace().next().map(str::to_string);
        } else if let Some(value) = field(line, "License") {
            package.license = Some(value.to_string());
        }
    }
    (!package.name.is_empty()).then_some(package)
}

/// The value of a `Key   Value` table row when the line starts with
/// exactly `key` followed by whitespace.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(key)?;
    rest.starts_with(char::is_whitespace)
        .then(|| rest.trim())
        .filter(|value| !value.is_empty())
}

/// `pear list-upgrades`: bordered table rows carrying local and remote
/// versions; the column layout varies across pear releases, so rows are
/// keyed on their two version-shaped tokens and the package name is the
/// token right before the first one.
fn parse_upgrades(stdout: &str) -> Vec<PearPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            if line.starts_with(char::is_whitespace) {
                return None;
            }
            let tokens: Vec<&str> = line.split_whitespace().collect();
            let mut versions = tokens
                .iter()
                .enumerate()
                .filter(|(_, token)| is_version(token));
            let (local_index, local) = versions.next()?;
            let (_, remote) = versions.next()?;
            if local_index == 0 {
                return None;
            }
            Some(PearPackage {
                name: tokens[local_index - 1].to_string(),
                version: Some(local.to_string()),
                latest_version: Some(remote.to_string()),
                state: InstallState::Upgradable,
                ..Default::default()
            })
        })
        .collect()
}

/// A package as pear describes it.
#[derive(Debug, Default)]
pub struct PearPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub origin: Option<String>,
    pub state: InstallState,
}

impl Package for PearPackage {
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
    fn parses_list_tables() {
        let stdout = "\
Installed packages, channel pear.php.net:
=========================================
Package          Version State
Archive_Tar      1.4.14  stable
Console_Getopt   1.4.3   stable
PEAR             1.10.13 stable
(no packages installed from channel pecl.php.net)
";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "Archive_Tar");
        assert_eq!(packages[0].version.as_deref(), Some("1.4.14"));
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn parses_search_rows() {
        let stdout = "\
Retrieving data...0%
Matched packages, channel pear.php.net:
=======================================
Package         Stable/(Latest) Local
HTTP            1.4.1 (stable)         Miscellaneous HTTP utilities
HTTP_Client     1.2.1 (stable)  1.2.0  Easy way to perform multiple HTTP requests
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "HTTP");
        assert_eq!(packages[0].version.as_deref(), Some("1.4.1"));
        assert_eq!(packages[0].state, InstallState::Available);
        assert_eq!(
            packages[0].description.as_deref(),
            Some("Miscellaneous HTTP utilities")
        );
        assert_eq!(packages[1].version.as_deref(), Some("1.2.0"));
        assert_eq!(packages[1].latest_version.as_deref(), Some("1.2.1"));
        assert_eq!(packages[1].state, InstallState::Upgradable);
    }

    #[test]
    fn parses_remote_info_for_uninstalled() {
        let stdout = "\
Package details:
================
Latest      1.4.14
Installed   - no -
Package     Archive_Tar
License     New BSD License
Category    File Formats
Summary     Tar file management class
Description This class provides handling of tar files in PHP.
";
        let package = parse_remote_info(stdout).unwrap();
        assert_eq!(package.name, "Archive_Tar");
        assert_eq!(package.version.as_deref(), Some("1.4.14"));
        assert_eq!(package.state, InstallState::Available);
        assert_eq!(
            package.description.as_deref(),
            Some("Tar file management class")
        );
    }

    #[test]
    fn parses_remote_info_for_outdated() {
        let stdout = "\
Package details:
================
Latest      1.10.13
Installed   1.10.12
Package     PEAR
License     New BSD License
Summary     PEAR Base System
";
        let package = parse_remote_info(stdout).unwrap();
        assert_eq!(package.version.as_deref(), Some("1.10.12"));
        assert_eq!(package.latest_version.as_deref(), Some("1.10.13"));
        assert_eq!(package.state, InstallState::Upgradable);
    }

    #[test]
    fn parses_info_table() {
        let stdout = "\
About pear.php.net/Archive_Tar-1.4.14
=====================================
Release Type          PEAR-style PHP-based Package
Name                  Archive_Tar
Channel               pear.php.net
Summary               Tar file management class
Description           This class provides handling of tar files in PHP.
Release Version       1.4.14 (stable)
License               New BSD License
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.name, "Archive_Tar");
        assert_eq!(package.origin.as_deref(), Some("pear.php.net"));
        assert_eq!(package.version.as_deref(), Some("1.4.14"));
        assert_eq!(package.license.as_deref(), Some("New BSD License"));
        assert_eq!(package.state, InstallState::Installed);
    }

    #[test]
    fn parses_upgrade_listing_with_channel_column() {
        let stdout = "\
Available upgrades (stable), channel pear.php.net:
==================================================
Channel          Package     Local           Remote          Size
pear.php.net     Archive_Tar 1.4.9 (stable)  1.4.14 (stable) 21kB
";
        let packages = parse_upgrades(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "Archive_Tar");
        assert_eq!(packages[0].version.as_deref(), Some("1.4.9"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("1.4.14"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn parses_upgrade_listing_without_channel_column() {
        let stdout = "\
Package     Local           Remote          Size
PEAR        1.10.12 (stable) 1.10.13 (stable) 295kB
";
        let packages = parse_upgrades(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "PEAR");
        assert_eq!(packages[0].version.as_deref(), Some("1.10.12"));
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("Archive_Tar@1.4.14")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("Archive_Tar")]).is_ok());
    }
}
