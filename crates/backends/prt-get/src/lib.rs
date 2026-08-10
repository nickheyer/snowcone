//! prt-get (CRUX ports) backend for snowcone.
//!
//! prt-get builds ports from source and never prompts, so `assume_yes` has
//! nothing to do; `--test` is its native dry run for install, update,
//! remove and sysup. Refreshing the ports tree belongs to the separate
//! `ports` binary (`ports -u`), resolved at startup. `prt-get info`
//! describes the ports tree side only, so a `listinst` probe fills in the
//! installed state, and `Version`/`Release` compose into CRUX's
//! `version-release` form.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "prt-get";
const PROGRAMS: &[&str] = &["prt-get"];
const PORTS_PROGRAM: &str = "ports";

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
            // Updating the ports tree is the separate `ports` binary's job;
            // resolved here so refresh can fail with a clear message when
            // it is missing.
            ports: find_program(PORTS_PROGRAM),
            elevator: Elevator::detect(host),
        }))
    }
}

struct Manager {
    program: PathBuf,
    ports: Option<PathBuf>,
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

    /// Elevated mutating command with `--test` when a dry run is asked for.
    /// prt-get never prompts, so `assume_yes` needs nothing.
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(subcommand).elevated(true);
        if ctx.dry_run {
            cmd = cmd.arg("--test");
        }
        cmd
    }
}

/// Ports build whatever version the tree carries: nothing to pin.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but ports build whatever version the ports tree carries"
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
        "prt-get (CRUX ports)"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "pkgutils"
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
            .args(["listinst", "-v"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_name_version(
            &output.stdout,
            InstallState::Installed,
        )))
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
        let mut package = parse_info(&output.stdout).ok_or_else(|| Error::Parse {
            what: format!("{ID} info output"),
            detail: format!("no `Name` field for `{name}`"),
        })?;
        // `info` reads the ports tree; one listinst probe fills in the
        // installed state and version.
        let list = self
            .query()
            .args(["listinst", "-v"])
            .capture(&self.elevator, None)
            .await?;
        if let Some(installed) = parse_name_version(&list.stdout, InstallState::Installed)
            .into_iter()
            .find(|installed| installed.name == package.name)
        {
            package.state = InstallState::Installed;
            if installed.version.is_some() && installed.version != package.version {
                package.latest_version = package.version.take();
                package.version = installed.version;
            }
        }
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["search", "-v"])
            .arg(query)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() && output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(boxed(parse_name_version(
            &output.stdout,
            InstallState::Available,
        )))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: refresh has no dry-run mode")));
        }
        let Some(ports) = &self.ports else {
            return Err(Error::Other(format!(
                "{ID}: refreshing the ports tree needs the separate `{PORTS_PROGRAM}` tool, \
                 which was not found on PATH"
            )));
        };
        self.run(Cmd::new(ports).arg("-u").elevated(true), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        let cmd = if packages.is_empty() {
            self.mutation("sysup", ctx)
        } else {
            self.mutation("update", ctx)
                .args(packages.iter().map(|package| package.name.as_str()))
        };
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("diff")
            .capture(&self.elevator, None)
            .await?;
        if output.stdout.trim().is_empty() {
            output.require_success()?;
            return Ok(Vec::new());
        }
        Ok(boxed(parse_diff(&output.stdout)))
    }
}

fn boxed(packages: Vec<PrtGetPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `listinst -v` / `search -v`: `name version` per line; prose lines are
/// skipped by requiring a digit in the version token.
fn parse_name_version(stdout: &str, state: InstallState) -> Vec<PrtGetPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            let (name, version) = match *tokens.as_slice() {
                [name] => (name, None),
                [name, version] if version.chars().any(|c| c.is_ascii_digit()) => {
                    (name, Some(version))
                }
                _ => return None,
            };
            Some(PrtGetPackage {
                name: name.to_string(),
                version: version.map(str::to_string),
                state,
                ..Default::default()
            })
        })
        .collect()
}

/// `prt-get diff`: header prose followed by `port installed available`
/// columns; requiring digits in both version columns filters the prose
/// (including the three-word `No differences found!`).
fn parse_diff(stdout: &str) -> Vec<PrtGetPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            let &[name, installed, available] = tokens.as_slice() else {
                return None;
            };
            let versioned = |token: &str| token.chars().any(|c| c.is_ascii_digit());
            if !versioned(installed) || !versioned(available) {
                return None;
            }
            Some(PrtGetPackage {
                name: name.to_string(),
                version: Some(installed.to_string()),
                latest_version: Some(available.to_string()),
                state: InstallState::Upgradable,
                ..Default::default()
            })
        })
        .collect()
}

/// `prt-get info`: `Key: Value` lines; `Version` and `Release` are
/// separate fields that compose into CRUX's `version-release` form, and
/// `Path` names the port's repository directory.
fn parse_info(stdout: &str) -> Option<PrtGetPackage> {
    let mut package = PrtGetPackage {
        state: InstallState::Available,
        ..Default::default()
    };
    let mut version = None;
    let mut release = None;
    for line in stdout.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if value.is_empty() {
            continue;
        }
        match key {
            "Name" => package.name = value.to_string(),
            "Version" => version = Some(value.to_string()),
            "Release" => release = Some(value.to_string()),
            "Description" => package.description = Some(value.to_string()),
            "URL" => package.homepage = Some(value.to_string()),
            "Path" => package.origin = Some(value.to_string()),
            "Dependencies" => {
                package.dependencies = Some(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|dep| !dep.is_empty())
                        .map(str::to_string)
                        .collect(),
                );
            }
            _ => {}
        }
    }
    package.version = match (version, release) {
        (Some(version), Some(release)) => Some(format!("{version}-{release}")),
        (version, None) => version,
        (None, Some(_)) => None,
    };
    (!package.name.is_empty()).then_some(package)
}

/// A package as prt-get describes it.
#[derive(Debug, Default)]
pub struct PrtGetPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub origin: Option<String>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for PrtGetPackage {
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

    fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
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
    fn parses_listinst_lines() {
        let packages = parse_name_version("gcc 12.2.0-1\nzsh 5.9-1\n", InstallState::Installed);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "zsh");
        assert_eq!(packages[1].version.as_deref(), Some("5.9-1"));
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn skips_prose_in_search_output() {
        let packages = parse_name_version(
            "No matching packages found\nripgrep 14.1.0-1\n",
            InstallState::Available,
        );
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "ripgrep");
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn parses_diff_columns() {
        let stdout = "\
Differences between installed packages and ports tree:

Port                Installed           Available in the ports tree

glibc               2.36-1              2.38-2
zsh                 5.9-1               5.9-2
";
        let packages = parse_diff(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "glibc");
        assert_eq!(packages[0].version.as_deref(), Some("2.36-1"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("2.38-2"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn diff_prose_parses_to_nothing() {
        assert!(parse_diff("No differences found!\n").is_empty());
    }

    #[test]
    fn parses_info_and_composes_version_release() {
        let stdout = "\
Name:         ripgrep
Path:         /usr/ports/contrib
Version:      14.1.0
Release:      1
Description:  Line-oriented search tool
URL:          https://github.com/BurntSushi/ripgrep
Dependencies: rust,pcre2
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.name, "ripgrep");
        assert_eq!(package.version.as_deref(), Some("14.1.0-1"));
        assert_eq!(package.origin.as_deref(), Some("/usr/ports/contrib"));
        assert_eq!(
            package.homepage.as_deref(),
            Some("https://github.com/BurntSushi/ripgrep")
        );
        assert_eq!(
            package.dependencies,
            Some(vec!["rust".to_string(), "pcre2".to_string()])
        );
        assert_eq!(package.state, InstallState::Available);
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("ripgrep@14.1.0")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("ripgrep")]).is_ok());
    }
}
