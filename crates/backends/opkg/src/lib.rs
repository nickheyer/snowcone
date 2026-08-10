//! opkg backend for snowcone.
//!
//! Drives OpenWrt's opkg. opkg never prompts, so `assume_yes` has nothing
//! to do, while `--noaction` (a global flag, given before the sub-command)
//! is a native dry run for install/remove/upgrade. `opkg upgrade` only
//! upgrades the packages it is named, so upgrade-all first collects
//! `list-upgradable`. Not every opkg build ships a search verb, so search
//! filters the full `opkg list` output client-side.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "opkg";
const PROGRAMS: &[&str] = &["opkg"];

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

    /// Elevated mutating command; `--noaction` is opkg's global dry-run
    /// flag and precedes the sub-command. opkg never prompts, so
    /// `assume_yes` needs nothing.
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().elevated(true);
        if ctx.dry_run {
            cmd = cmd.arg("--noaction");
        }
        cmd.arg(subcommand)
    }

    async fn upgradable(&self) -> Result<Vec<OpkgPackage>> {
        let output = self
            .query()
            .arg("list-upgradable")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_upgradable(&output.stdout))
    }
}

/// opkg has no version selection: installs take whatever the feeds carry.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but opkg installs whatever version the configured feeds carry"
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
        "opkg"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "opkg"
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
        Ok(boxed(parse_list(&output.stdout, InstallState::Installed)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query()
            .arg("info")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() || output.stdout.trim().is_empty() {
            return Err(Error::NotFound(name.to_string()));
        }
        let package = parse_info(&output.stdout).ok_or_else(|| Error::Parse {
            what: format!("{ID} info output"),
            detail: format!("no `Package` field for `{name}`"),
        })?;
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("list")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        let needle = query.to_lowercase();
        Ok(boxed(
            parse_list(&output.stdout, InstallState::Available)
                .into_iter()
                .filter(|package| matches(package, &needle))
                .collect(),
        ))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: refresh has no dry-run mode")));
        }
        self.run(self.cmd().arg("update").elevated(true), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        // `opkg upgrade` only touches the packages it is given; upgrade-all
        // means upgrading everything list-upgradable reports.
        let names: Vec<String> = if packages.is_empty() {
            self.upgradable()
                .await?
                .into_iter()
                .map(|package| package.name)
                .collect()
        } else {
            packages
                .iter()
                .map(|package| package.name.clone())
                .collect()
        };
        if names.is_empty() {
            return Ok(());
        }
        self.run(self.mutation("upgrade", ctx).args(names), ctx)
            .await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.upgradable().await?))
    }
}

fn boxed(packages: Vec<OpkgPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// Case-insensitive substring match on name or description; `needle` is
/// already lowercased.
fn matches(package: &OpkgPackage, needle: &str) -> bool {
    package.name.to_lowercase().contains(needle)
        || package
            .description
            .as_deref()
            .is_some_and(|description| description.to_lowercase().contains(needle))
}

/// `opkg list`/`list-installed`: `name - version[ - description]` lines;
/// the description may itself contain ` - `, so only the first two
/// separators split.
fn parse_list(stdout: &str, state: InstallState) -> Vec<OpkgPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, " - ");
            let name = parts.next()?.trim();
            let version = parts.next()?.trim();
            if name.is_empty() || name.contains(' ') || version.is_empty() {
                return None;
            }
            Some(OpkgPackage {
                name: name.to_string(),
                version: Some(version.to_string()),
                description: parts
                    .next()
                    .map(str::trim)
                    .filter(|description| !description.is_empty())
                    .map(str::to_string),
                state,
                ..Default::default()
            })
        })
        .collect()
}

/// `opkg list-upgradable`: `name - installed - available` lines.
fn parse_upgradable(stdout: &str) -> Vec<OpkgPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split(" - ").collect();
            let &[name, installed, available] = parts.as_slice() else {
                return None;
            };
            let name = name.trim();
            if name.is_empty() || name.contains(' ') {
                return None;
            }
            Some(OpkgPackage {
                name: name.to_string(),
                version: Some(installed.trim().to_string()),
                latest_version: Some(available.trim().to_string()),
                state: InstallState::Upgradable,
                ..Default::default()
            })
        })
        .collect()
}

/// `opkg info`: dpkg-style `Key: Value` stanzas (one per feed entry; only
/// the first is read); a `Status:` line whose words include `installed`
/// marks the local install, and sizes are plain byte counts.
fn parse_info(stdout: &str) -> Option<OpkgPackage> {
    let mut package = OpkgPackage {
        state: InstallState::Available,
        ..Default::default()
    };
    let mut seen_field = false;
    for line in stdout.lines() {
        if line.trim().is_empty() {
            if seen_field {
                break;
            }
            continue;
        }
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        seen_field = true;
        let (key, value) = (key.trim(), value.trim());
        if value.is_empty() {
            continue;
        }
        match key {
            "Package" => package.name = value.to_string(),
            "Version" => package.version = Some(value.to_string()),
            "Description" => package.description = Some(value.to_string()),
            "Section" => package.origin = Some(value.to_string()),
            "Architecture" => package.architecture = Some(value.to_string()),
            "Size" => package.download_size = value.parse().ok(),
            "Installed-Size" => package.installed_size = value.parse().ok(),
            "Depends" => {
                package.dependencies = Some(
                    value
                        .split(',')
                        .filter_map(|dep| dep.split_whitespace().next())
                        .map(str::to_string)
                        .collect(),
                );
            }
            "Status" if value.split_whitespace().any(|word| word == "installed") => {
                package.state = InstallState::Installed;
            }
            _ => {}
        }
    }
    (!package.name.is_empty()).then_some(package)
}

/// A package as opkg describes it.
#[derive(Debug, Default)]
pub struct OpkgPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub architecture: Option<String>,
    pub origin: Option<String>,
    pub installed_size: Option<u64>,
    pub download_size: Option<u64>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for OpkgPackage {
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
    fn parses_installed_list_lines() {
        let stdout = "\
busybox - 1.36.1-2
dropbear - 2022.82-2
";
        let packages = parse_list(stdout, InstallState::Installed);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "dropbear");
        assert_eq!(packages[1].version.as_deref(), Some("2022.82-2"));
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn parses_list_lines_with_descriptions() {
        let stdout =
            "tcpdump - 4.99.4-1 - Network monitoring and data acquisition tool - full variant\n";
        let packages = parse_list(stdout, InstallState::Available);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "tcpdump");
        assert_eq!(
            packages[0].description.as_deref(),
            Some("Network monitoring and data acquisition tool - full variant")
        );
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn parses_upgradable_lines() {
        let stdout = "dropbear - 2022.82-2 - 2022.83-1\n";
        let packages = parse_upgradable(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "dropbear");
        assert_eq!(packages[0].version.as_deref(), Some("2022.82-2"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("2022.83-1"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn parses_info_of_installed_package() {
        let stdout = "\
Package: dropbear
Version: 2022.82-2
Depends: libc, libgcc1
Status: install user installed
Section: net
Architecture: x86_64
Size: 112705
Installed-Size: 250880
Description: A small SSH 2 server/client designed for small memory environments.
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.name, "dropbear");
        assert_eq!(package.version.as_deref(), Some("2022.82-2"));
        assert_eq!(package.state, InstallState::Installed);
        assert_eq!(package.origin.as_deref(), Some("net"));
        assert_eq!(package.download_size, Some(112705));
        assert_eq!(package.installed_size, Some(250880));
        assert_eq!(
            package.dependencies,
            Some(vec!["libc".to_string(), "libgcc1".to_string()])
        );
    }

    #[test]
    fn info_without_status_stays_available() {
        let stdout = "\
Package: tcpdump
Version: 4.99.4-1
Section: net
Description: Network monitoring tool
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.state, InstallState::Available);
    }

    #[test]
    fn filters_search_matches_on_name_and_description() {
        let packages = parse_list(
            "tcpdump - 4.99.4-1 - Network monitoring tool\nbusybox - 1.36.1-2 - Core utilities\n",
            InstallState::Available,
        );
        assert!(matches(&packages[0], "tcp"));
        assert!(matches(&packages[0], "monitoring"));
        assert!(!matches(&packages[1], "tcp"));
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("dropbear@2022.82-2")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("dropbear")]).is_ok());
    }
}
