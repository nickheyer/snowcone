//! rebar3 backend for snowcone.
//!
//! rebar3 is project-scoped: dependency declarations live in
//! `rebar.config`, fetched sources and builds live under `_build`, and exact
//! resolutions live in `rebar.lock`. This backend fetches declared deps,
//! reads `rebar3 deps`, refreshes the Hex index, and upgrades through the
//! lock-aware `upgrade` provider. rebar3 cannot remove a declaration, so
//! removal reports the required manifest workflow rather than deleting an
//! arbitrary build directory behind the tool's back.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "rebar3";
const PROGRAMS: &[&str] = &["rebar3"];

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
        Cmd::new(&self.program).env("REBAR_COLOR", "none")
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

    async fn dependencies(&self) -> Result<Vec<Rebar3Package>> {
        let output = self
            .query()
            .arg("deps")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_deps(&output.stdout))
    }
}

fn reject_pins(packages: &[PackageRequest]) -> Result<()> {
    if let Some(package) = packages.iter().find(|package| package.version.is_some()) {
        Err(Error::Other(format!(
            "{ID}: `{package}` cannot be pinned on the command line; rebar.config owns the dependency constraint"
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
        "rebar3"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "hex"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::REFRESH | Capabilities::UPGRADE
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: install has no dry-run mode")));
        }
        self.run(self.cmd().arg("get-deps"), ctx).await?;
        if !packages.is_empty() {
            let installed = self.dependencies().await?;
            if let Some(package) = packages
                .iter()
                .find(|package| !installed.iter().any(|dep| dep.name == package.name))
            {
                return Err(Error::Other(format!(
                    "{ID}: `{}` is not declared in rebar.config",
                    package.name
                )));
            }
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        let target = packages
            .first()
            .map_or("a dependency", |package| package.name.as_str());
        Err(Error::Other(format!(
            "{ID}: cannot remove `{target}` by command; remove it from rebar.config, then run `rebar3 unlock {target}`"
        )))
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.dependencies().await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        self.dependencies()
            .await?
            .into_iter()
            .find(|package| package.name == name)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.into()))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: refresh has no dry-run mode")));
        }
        self.run(self.cmd().arg("update"), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: upgrade has no dry-run mode")));
        }
        let cmd = if packages.is_empty() {
            self.cmd().args(["upgrade", "--all"])
        } else {
            self.cmd().arg("upgrade").arg(
                packages
                    .iter()
                    .map(|package| package.name.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            )
        };
        self.run(cmd, ctx).await
    }
}

fn parse_deps(stdout: &str) -> Vec<Rebar3Package> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with("===>") || line.starts_with("-- ") {
                return None;
            }
            let (header, source) = line.split_once(" (")?;
            let source = source.strip_suffix(')')?;
            let mismatched = header.ends_with('*');
            let name = header.trim_end_matches('*').trim();
            if name.is_empty() || name.contains(' ') {
                return None;
            }
            let version = source
                .split("package ")
                .nth(1)
                .and_then(|value| value.split_whitespace().next())
                .map(|value| value.trim_end_matches([')', '<', '>']).to_string())
                .or_else(|| {
                    source
                        .strip_prefix("git source ")
                        .map(|value| value.to_string())
                });
            Some(Rebar3Package {
                name: name.into(),
                version,
                description: Some(source.into()),
                state: if mismatched {
                    InstallState::Upgradable
                } else {
                    InstallState::Installed
                },
            })
        })
        .collect()
}

fn boxed(packages: Vec<Rebar3Package>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// A dependency resolved by rebar3 for the current project profile.
#[derive(Debug)]
pub struct Rebar3Package {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for Rebar3Package {
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
    fn parses_locked_package_and_source_dependencies() {
        let output = "===> Verifying dependencies...\ncowboy (locked package 2.13.0)\nranch* (package 2.1.0)\ncustom (git source abc1234...)\n";
        let packages = parse_deps(output);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "cowboy");
        assert_eq!(packages[0].version.as_deref(), Some("2.13.0"));
        assert_eq!(packages[1].state, InstallState::Upgradable);
        assert_eq!(packages[2].version.as_deref(), Some("abc1234..."));
    }

    #[test]
    fn rejects_cli_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("cowboy@2.13.0")]).is_err());
    }
}
