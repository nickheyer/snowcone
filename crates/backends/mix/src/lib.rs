//! Mix + Hex backend for snowcone.
//!
//! This backend manages the current Mix project's declared dependencies.
//! Mix owns `mix.exs`, `mix.lock`, `deps/`, and `_build/`; installs use
//! `deps.get`, removals use `deps.clean --unlock`, and upgrades use
//! `deps.update`. Hex's package commands provide remote search and info.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "mix";
const PROGRAMS: &[&str] = &["mix"];

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
        Cmd::new(&self.program).env("NO_COLOR", "1")
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

    async fn dependencies(&self) -> Result<Vec<MixPackage>> {
        let output = self
            .query()
            .arg("deps")
            .arg("--all")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_deps(&output.stdout))
    }

    async fn hex_info(&self, name: &str) -> Result<MixPackage> {
        let output = self
            .query()
            .args(["hex.info", name])
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.into()));
        }
        parse_hex_info(&output.stdout, name).ok_or_else(|| Error::NotFound(name.into()))
    }
}

fn reject_pins(packages: &[PackageRequest]) -> Result<()> {
    if let Some(package) = packages.iter().find(|package| package.version.is_some()) {
        Err(Error::Other(format!(
            "{ID}: `{package}` cannot be pinned on the command line; mix.exs owns the version requirement"
        )))
    } else {
        Ok(())
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Mix + Hex"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "hex"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::SEARCH | Capabilities::UPGRADE
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: install has no dry-run mode")));
        }
        if !packages.is_empty() {
            let declared = self.dependencies().await?;
            if let Some(package) = packages
                .iter()
                .find(|package| !declared.iter().any(|dep| dep.name == package.name))
            {
                return Err(Error::Other(format!(
                    "{ID}: `{}` is not declared in mix.exs; add it to deps/0 before installing",
                    package.name
                )));
            }
        }
        // deps.get always resolves every out-of-date declared dependency;
        // Mix has no safe per-name fetch mode.
        self.run(self.cmd().args(["deps.get"]), ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: remove has no dry-run mode")));
        }
        let cmd = if packages.is_empty() {
            self.cmd().args(["deps.clean", "--all", "--unlock"])
        } else {
            self.cmd()
                .arg("deps.clean")
                .args(packages.iter().map(|package| package.name.as_str()))
                .arg("--unlock")
        };
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(
            self.dependencies()
                .await?
                .into_iter()
                .filter(|package| package.state == InstallState::Installed)
                .collect(),
        ))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let installed = self
            .dependencies()
            .await?
            .into_iter()
            .find(|package| package.name == name);
        let mut package = self.hex_info(name).await?;
        if let Some(installed) = installed {
            package.version = installed.version;
            package.state = installed.state;
        }
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["hex.package", "search", query])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_hex_search(&output.stdout)))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: upgrade has no dry-run mode")));
        }
        let cmd = if packages.is_empty() {
            self.cmd().args(["deps.update", "--all"])
        } else {
            self.cmd()
                .arg("deps.update")
                .args(packages.iter().map(|package| package.name.as_str()))
        };
        self.run(cmd, ctx).await
    }
}

fn parse_deps(stdout: &str) -> Vec<MixPackage> {
    let mut packages = Vec::new();
    let mut current: Option<MixPackage> = None;
    for line in stdout.lines() {
        if let Some(header) = line.trim_start().strip_prefix("* ") {
            if let Some(package) = current.take() {
                packages.push(package);
            }
            let mut parts = header.split_whitespace();
            let Some(name) = parts.next() else { continue };
            let version = parts
                .next()
                .filter(|value| !value.starts_with('('))
                .map(str::to_string);
            current = Some(MixPackage {
                name: name.to_string(),
                version,
                description: None,
                state: InstallState::Installed,
            });
            continue;
        }
        if let Some(package) = current.as_mut()
            && line.contains("dependency is not available")
        {
            package.state = InstallState::Available;
        }
    }
    if let Some(package) = current {
        packages.push(package);
    }
    packages
}

fn split_columns(line: &str) -> Vec<&str> {
    let bytes = line.as_bytes();
    let mut columns = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b' ' {
            let gap = index;
            while index < bytes.len() && bytes[index] == b' ' {
                index += 1;
            }
            if index - gap >= 2 {
                let value = line[start..gap].trim();
                if !value.is_empty() {
                    columns.push(value);
                }
                start = index;
            }
        } else {
            index += 1;
        }
    }
    let value = line[start..].trim();
    if !value.is_empty() {
        columns.push(value);
    }
    columns
}

fn parse_hex_search(stdout: &str) -> Vec<MixPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let columns = split_columns(line);
            if columns.len() < 4 || columns[0] == "Package" || columns[0] == "Organization" {
                return None;
            }
            let (name_index, description_index, version_index) = if columns.len() >= 5 {
                (1, 2, 3)
            } else {
                (0, 1, 2)
            };
            Some(MixPackage {
                name: columns.get(name_index)?.to_string(),
                version: columns.get(version_index).map(|value| value.to_string()),
                description: columns
                    .get(description_index)
                    .map(|value| value.to_string()),
                state: InstallState::Available,
            })
        })
        .collect()
}

fn parse_hex_info(stdout: &str, fallback: &str) -> Option<MixPackage> {
    let description = stdout
        .lines()
        .take_while(|line| !line.trim().is_empty())
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("Config:"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut in_releases = false;
    let mut version = None;
    for line in stdout.lines() {
        if line.trim() == "Recent releases:" {
            in_releases = true;
            continue;
        }
        if in_releases {
            let candidate = line.split_whitespace().next();
            if candidate.is_some_and(|value| {
                value
                    .chars()
                    .next()
                    .is_some_and(|character| character.is_ascii_digit())
            }) {
                version = candidate.map(str::to_string);
                break;
            }
        }
    }
    (!description.is_empty() || version.is_some()).then(|| MixPackage {
        name: fallback.into(),
        version,
        description: (!description.is_empty()).then_some(description),
        state: InstallState::Available,
    })
}

fn boxed(packages: Vec<MixPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// A dependency in the current Mix project or a package available from Hex.
#[derive(Debug)]
pub struct MixPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for MixPackage {
    fn manager(&self) -> &str {
        ID
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> Option<&str> {
        self.version.as_deref()
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
    fn parses_mix_dependency_status_blocks() {
        let output = "* plug 1.16.1 (Hex package) (mix)\n  locked at 1.16.1\n  ok\n* missing 0.2.0 (Hex package)\n  the dependency is not available, run mix deps.get\n";
        let packages = parse_deps(output);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "plug");
        assert_eq!(packages[0].version.as_deref(), Some("1.16.1"));
        assert_eq!(packages[1].state, InstallState::Available);
    }

    #[test]
    fn parses_hex_search_tables() {
        let output = "Package  Description       Version  URL\nplug     A specification   1.16.1   https://hex.pm/packages/plug\necto     Database wrapper  3.13.0   https://hex.pm/packages/ecto\n";
        let packages = parse_hex_search(output);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "plug");
        assert_eq!(packages[0].version.as_deref(), Some("1.16.1"));
    }

    #[test]
    fn parses_hex_package_info() {
        let output = "Composable modules for web applications\n\nConfig: {:plug, \"~> 1.16\"}\n\nRecent releases:\n  1.16.1 (2024-03-01)\n  1.16.0 (2024-01-01)\n";
        let package = parse_hex_info(output, "plug").unwrap();
        assert_eq!(package.name, "plug");
        assert_eq!(package.version.as_deref(), Some("1.16.1"));
        assert_eq!(
            package.description.as_deref(),
            Some("Composable modules for web applications")
        );
    }

    #[test]
    fn rejects_command_line_pins() {
        assert!(reject_pins(&[PackageRequest::parse("plug@1.16.1")]).is_err());
    }
}
