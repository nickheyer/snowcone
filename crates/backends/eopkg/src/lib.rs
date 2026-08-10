//! eopkg backend for snowcone.
//!
//! Solus's fork of PiSi. Mutations run through the elevation helper with
//! `--yes-all` answering prompts and a native `--dry-run` on install,
//! remove, and upgrade. The listing verbs (`list-installed`,
//! `list-upgrades`, `search`) print `name - summary` lines with no version
//! column, so those results stay version-less rather than paying one
//! `info` call per package. `eopkg info` prints `Key : Value` stanzas
//! under an "Installed package:" and a "Package found in <repo>
//! repository:" header, so info knows the install state without a second
//! probe. Versions render as `version-release` - the identity Solus itself
//! uses (release bumps ship updates even when the version string doesn't
//! move).

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "eopkg";
const PROGRAMS: &[&str] = &["eopkg"];

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

    /// Shared flags for mutating commands: `--yes-all` and the native
    /// `--dry-run`.
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(subcommand).elevated(true);
        if ctx.assume_yes {
            cmd = cmd.arg("--yes-all");
        }
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        cmd
    }
}

/// eopkg has no version selection: installs always take the repository's
/// current version-release.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but eopkg always installs the repository's current version"
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
        "eopkg"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "eopkg"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::REFRESH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
    }

    fn needs_elevation(&self, operation: Operation) -> bool {
        matches!(
            operation,
            Operation::Install | Operation::Remove | Operation::Upgrade | Operation::Refresh
        )
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        let cmd = self
            .mutation("install", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = self
            .mutation("remove", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("list-installed")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_listing(&output.stdout, InstallState::Installed)))
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
        parse_info(&output.stdout)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("search")
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_listing(&output.stdout, InstallState::Available)))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: refresh has no dry-run mode")));
        }
        self.run(self.cmd().arg("update-repo").elevated(true), ctx)
            .await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
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
        Ok(boxed(parse_listing(
            &output.stdout,
            InstallState::Upgradable,
        )))
    }
}

fn boxed(packages: Vec<EopkgPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `name - summary` listing lines (the name column may be space-padded);
/// header and notice lines lack the ` - ` separator and are skipped.
fn parse_listing(stdout: &str, state: InstallState) -> Vec<EopkgPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let (name, summary) = line.split_once(" - ")?;
            let name = name.trim();
            if name.is_empty() || name.contains(char::is_whitespace) {
                return None;
            }
            Some(EopkgPackage {
                name: name.to_string(),
                description: Some(summary.trim().to_string()).filter(|s| !s.is_empty()),
                state,
                ..Default::default()
            })
        })
        .collect()
}

/// `4.45 MB`-style sizes: eopkg divides by 1024 but labels KB/MB/GB.
fn parse_size(value: &str) -> Option<u64> {
    let mut parts = value.split_whitespace();
    let number: f64 = parts.next()?.replace(',', "").parse().ok()?;
    let factor = match parts.next().unwrap_or("B") {
        "B" | "bytes" => 1.0,
        "KB" | "KiB" => 1024.0,
        "MB" | "MiB" => 1024.0 * 1024.0,
        "GB" | "GiB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((number * factor) as u64)
}

/// `eopkg info` stanzas: `Key : Value` fields under an `Installed
/// package:` and/or `Package found in <repo> repository:` header. The
/// `Name` line carries `name, version: X, release: Y` inline; versions
/// render as `X-Y` because the release is part of a Solus package's
/// identity.
fn parse_info(stdout: &str) -> Option<EopkgPackage> {
    let mut sections: Vec<(bool, Option<String>, EopkgPackage)> = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed == "Installed package:" {
            sections.push((true, None, EopkgPackage::default()));
            continue;
        }
        if let Some(repo) = trimmed
            .strip_prefix("Package found in ")
            .and_then(|rest| rest.strip_suffix(" repository:"))
        {
            sections.push((false, Some(repo.to_string()), EopkgPackage::default()));
            continue;
        }
        let Some((_, _, package)) = sections.last_mut() else {
            continue;
        };
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if value.is_empty() {
            continue;
        }
        match key {
            "Name" => {
                let mut parts = value.split(',');
                package.name = parts.next().unwrap_or_default().trim().to_string();
                let (mut version, mut release) = (None, None);
                for part in parts {
                    let Some((k, v)) = part.split_once(':') else {
                        continue;
                    };
                    match k.trim() {
                        "version" => version = Some(v.trim()),
                        "release" => release = Some(v.trim()),
                        _ => {}
                    }
                }
                package.version = match (version, release) {
                    (Some(version), Some(release)) => Some(format!("{version}-{release}")),
                    (Some(version), None) => Some(version.to_string()),
                    _ => None,
                };
            }
            "Summary" => package.description = Some(value.to_string()),
            "Description" if package.description.is_none() => {
                package.description = Some(value.to_string());
            }
            "Licenses" => package.license = Some(value.to_string()),
            "Architecture" => package.architecture = Some(value.to_string()),
            "Dependencies" => {
                let dependencies: Vec<String> = value
                    .split_whitespace()
                    .map(|dep| dep.trim_end_matches(',').to_string())
                    .collect();
                package.dependencies = Some(dependencies).filter(|deps| !deps.is_empty());
            }
            "Installed Size" => package.installed_size = parse_size(value),
            "Package Size" => package.download_size = parse_size(value),
            _ => {}
        }
    }
    let mut installed: Option<EopkgPackage> = None;
    let mut available: Option<(Option<String>, EopkgPackage)> = None;
    for (is_installed, repo, package) in sections {
        if package.name.is_empty() {
            continue;
        }
        if is_installed {
            installed.get_or_insert(package);
        } else if available.is_none() {
            available = Some((repo, package));
        }
    }
    match (installed, available) {
        (Some(mut package), Some((repo, repo_package))) => {
            package.origin = repo;
            if repo_package.version != package.version {
                package.latest_version = repo_package.version;
                package.state = InstallState::Upgradable;
            } else {
                package.state = InstallState::Installed;
            }
            if package.download_size.is_none() {
                package.download_size = repo_package.download_size;
            }
            Some(package)
        }
        (Some(mut package), None) => {
            package.state = InstallState::Installed;
            Some(package)
        }
        (None, Some((repo, mut package))) => {
            package.origin = repo;
            package.state = InstallState::Available;
            Some(package)
        }
        (None, None) => None,
    }
}

/// A package as eopkg describes it.
#[derive(Debug, Default)]
pub struct EopkgPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub architecture: Option<String>,
    pub origin: Option<String>,
    pub installed_size: Option<u64>,
    pub download_size: Option<u64>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for EopkgPackage {
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

    fn architecture(&self) -> Option<&str> {
        self.architecture.as_deref()
    }

    fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
    }

    fn installed_size(&self) -> Option<u64> {
        self.installed_size
    }

    fn download_size(&self) -> Option<u64> {
        self.download_size
    }

    fn dependencies(&self) -> Option<Vec<String>> {
        self.dependencies.clone()
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_installed_listing() {
        let stdout = "\
Installed packages:
acl        - Access control list utilities
ripgrep    - Line oriented search tool
";
        let packages = parse_listing(stdout, InstallState::Installed);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "ripgrep");
        assert_eq!(
            packages[1].description.as_deref(),
            Some("Line oriented search tool")
        );
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn listing_skips_notices() {
        let packages = parse_listing("No packages to upgrade.\n", InstallState::Upgradable);
        assert!(packages.is_empty());
    }

    #[test]
    fn parses_info_with_both_sections() {
        let stdout = "\
Installed package:
Name                : ripgrep, version: 13.0.0, release: 15
Summary             : Line oriented search tool
Description         : ripgrep is a line-oriented search tool that recursively
searches your current directory for a regex pattern.
Licenses            : MIT
Component           : system.utils
Dependencies        :
Distribution        : Solus, Dist. Release: 1
Architecture        : x86_64
Installed Size      : 4.45 MB
Reverse Dependencies:

Package found in Solus repository:
Name                : ripgrep, version: 14.1.0, release: 21
Summary             : Line oriented search tool
Licenses            : MIT
Component           : system.utils
Dependencies        : pcre2
Architecture        : x86_64
Package Size        : 1.72 MB
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.name, "ripgrep");
        assert_eq!(package.version.as_deref(), Some("13.0.0-15"));
        assert_eq!(package.latest_version.as_deref(), Some("14.1.0-21"));
        assert_eq!(package.state, InstallState::Upgradable);
        assert_eq!(package.origin.as_deref(), Some("Solus"));
        assert_eq!(package.license.as_deref(), Some("MIT"));
        assert_eq!(package.architecture.as_deref(), Some("x86_64"));
        assert_eq!(
            package.description.as_deref(),
            Some("Line oriented search tool")
        );
        assert_eq!(package.installed_size, Some((4.45 * 1024.0 * 1024.0) as u64));
        assert_eq!(package.download_size, Some((1.72 * 1024.0 * 1024.0) as u64));
    }

    #[test]
    fn parses_info_repo_only() {
        let stdout = "\
Package found in Solus repository:
Name                : nano, version: 8.0, release: 142
Summary             : Small, friendly text editor
Licenses            : GPL-3.0-or-later
Dependencies        : ncurses zlib
Architecture        : x86_64
Package Size        : 708.00 KB
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.name, "nano");
        assert_eq!(package.version.as_deref(), Some("8.0-142"));
        assert_eq!(package.state, InstallState::Available);
        assert_eq!(
            package.dependencies,
            Some(vec!["ncurses".to_string(), "zlib".to_string()])
        );
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("708.00 KB"), Some((708.0 * 1024.0) as u64));
        assert_eq!(parse_size("1.72 MB"), Some((1.72 * 1024.0 * 1024.0) as u64));
        assert_eq!(parse_size("nonsense"), None);
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("ripgrep@14.1.0")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("ripgrep")]).is_ok());
    }
}
