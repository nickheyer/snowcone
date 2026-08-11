//! NuGet (dotnet) backend for snowcone.
//!
//! Manages .NET *global tools* (`dotnet tool … --global`); the per-project
//! package world belongs to project tooling, not a system package CLI. The
//! tool CLI prints aligned tables, so parsers key off the dashed separator
//! line under `LC_ALL=C` (the SDK localizes its output), with
//! `DOTNET_NOLOGO` set because the first-run welcome banner contains dashed
//! lines of its own. dotnet never prompts and no tool verb has a dry-run.
//! A full upgrade goes through `dotnet tool update --all` (SDK 7+), falling
//! back to one `dotnet tool update` per installed tool on SDKs that reject
//! the flag. There is no native outdated verb: outdated compares each
//! installed tool against the registry's latest via `dotnet tool search`.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "dotnet";
const PROGRAMS: &[&str] = &["dotnet"];

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

    /// Read invocation with a stable locale and no first-run banner, so the
    /// table parsers see only the table.
    fn query(&self) -> Cmd {
        Cmd::new(&self.program)
            .env("LC_ALL", "C")
            .env("DOTNET_NOLOGO", "1")
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

    /// Globally installed tools, from the `dotnet tool list --global` table.
    async fn installed(&self) -> Result<Vec<DotnetPackage>> {
        let output = self
            .query()
            .args(["tool", "list", "--global"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_table(&output.stdout, InstallState::Installed))
    }

    /// Registry matches for a query, from the `dotnet tool search` table.
    /// `take` widens the paging past the API's default of 20 results.
    async fn search_registry(&self, query: &str, take: Option<u32>) -> Result<Vec<DotnetPackage>> {
        let mut cmd = self.query().args(["tool", "search"]).arg(query);
        if let Some(take) = take {
            cmd = cmd.arg("--take").arg(take.to_string());
        }
        let output = cmd.capture(&self.elevator, None).await?.require_success()?;
        Ok(parse_table(&output.stdout, InstallState::Available))
    }
}

/// The full-width `----` line separating a dotnet table header from its
/// rows.
fn is_separator(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && trimmed.chars().all(|c| c == '-')
}

/// `dotnet tool list` / `dotnet tool search` tables: a column header, a
/// dashed separator, then one row per tool whose first two columns are the
/// package id and a version (the trailing columns - commands, authors,
/// downloads - are not carried).
fn parse_table(stdout: &str, state: InstallState) -> Vec<DotnetPackage> {
    stdout
        .lines()
        .skip_while(|line| !is_separator(line))
        .skip(1)
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            Some(DotnetPackage {
                name: parts.next()?.to_string(),
                version: parts.next().map(str::to_string),
                state,
                ..Default::default()
            })
        })
        .collect()
}

/// `dotnet tool search --detail`: `----------------`-separated blocks with
/// the package id alone on the first line, `Key: Value` pairs after it, and
/// an indented per-version list that is skipped.
fn parse_detail(stdout: &str) -> Vec<DotnetPackage> {
    let mut packages: Vec<DotnetPackage> = Vec::new();
    let mut expect_id = false;
    for line in stdout.lines() {
        if is_separator(line) {
            expect_id = true;
            continue;
        }
        if expect_id {
            let id = line.trim();
            if !id.is_empty() {
                packages.push(DotnetPackage {
                    name: id.to_string(),
                    state: InstallState::Available,
                    ..Default::default()
                });
                expect_id = false;
            }
            continue;
        }
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some(package) = packages.last_mut() else {
            continue;
        };
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "Latest Version" => package.version = Some(value.to_string()),
            "Summary" if package.description.is_none() => {
                package.description = Some(value.to_string());
            }
            "Description" => package.description = Some(value.to_string()),
            _ => {}
        }
    }
    packages
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "NuGet (dotnet)"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "nuget"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: install has no dry-run mode")));
        }
        // `dotnet tool install` takes one package id per invocation.
        for package in packages {
            let mut cmd = self.cmd().args(["tool", "install", "--global"]);
            if let Some(version) = &package.version {
                cmd = cmd.arg("--version").arg(version);
            }
            self.run(cmd.arg(&package.name), ctx).await?;
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: uninstall has no dry-run mode")));
        }
        for package in packages {
            let cmd = self
                .cmd()
                .args(["tool", "uninstall", "--global"])
                .arg(&package.name);
            self.run(cmd, ctx).await?;
        }
        Ok(())
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .installed()
            .await?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // A failed search (offline, unknown id) just means no registry
        // half; the installed table may still answer. NuGet ids compare
        // case-insensitively.
        let output = self
            .query()
            .args(["tool", "search", "--detail"])
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        let mut package = parse_detail(&output.stdout)
            .into_iter()
            .find(|package| package.name.eq_ignore_ascii_case(name));
        if let Some(installed) = self
            .installed()
            .await?
            .into_iter()
            .find(|installed| installed.name.eq_ignore_ascii_case(name))
        {
            package = Some(match package {
                Some(mut package) => {
                    if installed.version.is_some() && installed.version != package.version {
                        package.state = InstallState::Upgradable;
                        package.latest_version = package.version.take();
                    } else {
                        package.state = InstallState::Installed;
                    }
                    package.version = installed.version;
                    package
                }
                None => installed,
            });
        }
        package
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .search_registry(query, None)
            .await?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: update has no dry-run mode")));
        }
        if packages.is_empty() {
            // `--all` (SDK 7+) updates the whole set in one pass. SDKs too
            // old to know the flag exit non-zero immediately, so any
            // failure falls back to one `tool update` per installed tool -
            // update is idempotent, so a re-run after a mid-set failure
            // only repeats no-op updates.
            let all = self.cmd().args(["tool", "update", "--global", "--all"]);
            if self.run(all, ctx).await.is_ok() {
                return Ok(());
            }
            for package in self.installed().await? {
                let cmd = self
                    .cmd()
                    .args(["tool", "update", "--global"])
                    .arg(&package.name);
                self.run(cmd, ctx).await?;
            }
            return Ok(());
        }
        for package in packages {
            let mut cmd = self.cmd().args(["tool", "update", "--global"]);
            if let Some(version) = &package.version {
                cmd = cmd.arg("--version").arg(version);
            }
            self.run(cmd.arg(&package.name), ctx).await?;
        }
        Ok(())
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        // No native outdated verb: ask the registry for each installed
        // tool's latest version and compare. `--take` widens the paging
        // well past the 20-result default so an exact id is unlikely to
        // fall off the page; a tool that still doesn't surface in its own
        // search results is silently skipped.
        let mut outdated = Vec::new();
        for installed in self.installed().await? {
            let latest = self
                .search_registry(&installed.name, Some(200))
                .await?
                .into_iter()
                .find(|found| found.name.eq_ignore_ascii_case(&installed.name))
                .and_then(|found| found.version);
            if let Some(latest) = latest
                && installed.version.as_deref() != Some(latest.as_str())
            {
                outdated.push(Box::new(DotnetPackage {
                    latest_version: Some(latest),
                    state: InstallState::Upgradable,
                    ..installed
                }) as Box<dyn Package>);
            }
        }
        Ok(outdated)
    }
}

/// A global tool as the dotnet CLI describes it.
#[derive(Debug, Default)]
pub struct DotnetPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for DotnetPackage {
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

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_installed_list_table() {
        let stdout = "\
Package Id        Version      Commands
-----------------------------------------
dotnetsay         2.1.7        dotnetsay
dotnet-ef         8.0.1        dotnet-ef
";
        let packages = parse_table(stdout, InstallState::Installed);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "dotnetsay");
        assert_eq!(packages[0].version.as_deref(), Some("2.1.7"));
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[1].name, "dotnet-ef");
    }

    #[test]
    fn parses_search_table() {
        let stdout = "\
Package ID          Latest Version      Authors            Downloads      Verified
------------------------------------------------------------------------------------
dotnetsay           2.1.7               nocture            21312312
dotnet-ef           8.0.1               Microsoft          123456789          x
";
        let packages = parse_table(stdout, InstallState::Available);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "dotnet-ef");
        assert_eq!(packages[1].version.as_deref(), Some("8.0.1"));
        assert_eq!(packages[1].state, InstallState::Available);
    }

    #[test]
    fn headerless_output_parses_to_nothing() {
        assert!(parse_table("No tools installed.\n", InstallState::Installed).is_empty());
    }

    #[test]
    fn parses_search_detail_blocks() {
        let stdout = "\
----------------
dotnet-format
Latest Version: 4.1.131201
Authors: Microsoft
Tags:
Downloads: 496746
Verified: False
Summary: Command line formatter.
Description: Command line tool for formatting code files based on .editorconfig settings.
Versions:
        3.0.2 Downloads: 1240
        4.1.131201 Downloads: 25
----------------
dotnet-format-lite
Latest Version: 1.0.0
Summary: Another formatter.
";
        let packages = parse_detail(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "dotnet-format");
        assert_eq!(packages[0].version.as_deref(), Some("4.1.131201"));
        assert_eq!(
            packages[0].description.as_deref(),
            Some("Command line tool for formatting code files based on .editorconfig settings.")
        );
        assert_eq!(packages[1].name, "dotnet-format-lite");
        assert_eq!(
            packages[1].description.as_deref(),
            Some("Another formatter.")
        );
        assert_eq!(packages[1].state, InstallState::Available);
    }
}
