//! Paket backend for snowcone.
//!
//! Paket manages the NuGet dependencies declared by the current solution's
//! `paket.dependencies` file. Resolved direct and transitive packages come
//! from Paket's tooling output, and mutations are delegated to `add`,
//! `remove`, and `update` so Paket remains the sole owner of its manifests.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "paket";
const PROGRAMS: &[&str] = &["paket"];

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
        let program = find_program(PROGRAMS[0]).ok_or_else(|| Error::Unavailable(ID.into()))?;
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

    fn query(&self) -> Cmd {
        self.cmd().env("LC_ALL", "C")
    }

    async fn run(&self, cmd: Cmd, ctx: &OpContext) -> Result<()> {
        let output = match &ctx.events {
            Some(events) => cmd.capture(&self.elevator, Some(events)).await?,
            None => cmd.run_interactive(&self.elevator).await?,
        };
        output.require_success()?;
        Ok(())
    }

    async fn installed(&self) -> Result<Vec<PaketPackage>> {
        let output = self
            .query()
            .args(["show-installed-packages", "--all", "--silent"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_installed(&output.stdout))
    }

    async fn newest_version(&self, name: &str) -> Result<Option<String>> {
        let output = self
            .query()
            .args(["find-package-versions", name, "--silent", "--max", "1"])
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Ok(None);
        }
        Ok(output
            .stdout
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with("Paket version"))
            .map(str::to_string))
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Paket"
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
        if packages.is_empty() {
            return self.run(self.cmd().arg("install"), ctx).await;
        }
        for package in packages {
            let mut cmd = self.cmd().arg("add").arg(&package.name);
            if let Some(version) = &package.version {
                cmd = cmd.args(["--version", version]);
            }
            self.run(cmd, ctx).await?;
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if packages.is_empty() {
            return Err(Error::Other(format!(
                "{ID}: remove requires at least one package"
            )));
        }
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: remove has no dry-run mode")));
        }
        for package in packages {
            self.run(self.cmd().arg("remove").arg(&package.name), ctx)
                .await?;
        }
        Ok(())
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.installed().await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let mut package = self
            .installed()
            .await?
            .into_iter()
            .find(|package| package.name.eq_ignore_ascii_case(name));
        let newest = self.newest_version(name).await?;
        if let Some(installed) = package.as_mut()
            && newest.as_deref() != installed.version.as_deref()
            && newest.is_some()
        {
            installed.latest_version = newest;
            installed.state = InstallState::Upgradable;
        } else if package.is_none() {
            package = newest.map(|version| PaketPackage {
                name: name.into(),
                version: Some(version),
                latest_version: None,
                description: None,
                group: None,
                state: InstallState::Available,
            });
        }
        package
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.into()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["find-packages", query, "--silent", "--max", "100"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_search(&output.stdout)))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: upgrade has no dry-run mode")));
        }
        if packages.is_empty() {
            return self.run(self.cmd().arg("update"), ctx).await;
        }
        for package in packages {
            let mut cmd = self.cmd().arg("update").arg(&package.name);
            if let Some(version) = &package.version {
                cmd = cmd.args(["--version", version]);
            }
            self.run(cmd, ctx).await?;
        }
        Ok(())
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["outdated", "--silent"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_outdated(&output.stdout)))
    }
}

fn parse_installed(stdout: &str) -> Vec<PaketPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let group = fields.next()?;
            let name = fields.next()?;
            if fields.next()? != "-" {
                return None;
            }
            let version = fields.next()?;
            if fields.next().is_some() {
                return None;
            }
            Some(PaketPackage {
                name: name.into(),
                version: Some(version.into()),
                latest_version: None,
                description: None,
                group: Some(group.into()),
                state: InstallState::Installed,
            })
        })
        .collect()
}

fn parse_search(stdout: &str) -> Vec<PaketPackage> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty()
                && !line.starts_with("Paket version")
                && !line.chars().any(char::is_whitespace)
        })
        .map(|name| PaketPackage {
            name: name.into(),
            version: None,
            latest_version: None,
            description: None,
            group: None,
            state: InstallState::Available,
        })
        .collect()
}

fn parse_outdated(stdout: &str) -> Vec<PaketPackage> {
    let mut group = None;
    let mut packages = Vec::new();
    for line in stdout.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("Group:") {
            group = Some(value.trim().to_string());
            continue;
        }
        let Some(change) = line.strip_prefix("* ") else {
            continue;
        };
        let Some((current, newest)) = change.split_once(" -> ") else {
            continue;
        };
        let Some((name, version)) = current.rsplit_once(' ') else {
            continue;
        };
        packages.push(PaketPackage {
            name: name.into(),
            version: Some(version.into()),
            latest_version: Some(newest.into()),
            description: None,
            group: group.clone(),
            state: InstallState::Upgradable,
        });
    }
    packages
}

fn boxed(packages: Vec<PaketPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// A NuGet package resolved by Paket for the current dependency group.
#[derive(Debug)]
pub struct PaketPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub group: Option<String>,
    pub state: InstallState,
}

impl Package for PaketPackage {
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
    fn parses_all_installed_packages_and_groups() {
        let output = "Main Newtonsoft.Json - 13.0.3\nBuild FAKE - 6.1.3\n";
        let packages = parse_installed(output);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "Newtonsoft.Json");
        assert_eq!(packages[0].version.as_deref(), Some("13.0.3"));
        assert_eq!(packages[1].group.as_deref(), Some("Build"));
    }

    #[test]
    fn parses_search_tooling_output() {
        let packages = parse_search("FAKE\nFAKE.Core\n\n");
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "FAKE.Core");
        assert_eq!(packages[1].state, InstallState::Available);
    }

    #[test]
    fn parses_outdated_packages() {
        let output = "Outdated packages found:\n  Group: Main\n    * Castle.Core 4.4.0 -> 5.1.1\n  Group: Build\n    * FAKE 5.23.1 -> 6.1.3\n";
        let packages = parse_outdated(output);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].latest_version.as_deref(), Some("5.1.1"));
        assert_eq!(packages[1].group.as_deref(), Some("Build"));
    }
}
