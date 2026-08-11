//! vcpkg backend for snowcone.
//!
//! Implements vcpkg's classic-mode package operations.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "vcpkg";
const PROGRAMS: &[&str] = &["vcpkg"];

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
        Cmd::new(&self.program).env("VCPKG_DISABLE_METRICS", "1")
    }
    async fn run(&self, cmd: Cmd, ctx: &OpContext) -> Result<()> {
        let out = match &ctx.events {
            Some(e) => cmd.capture(&self.elevator, Some(e)).await?,
            None => cmd.run_interactive(&self.elevator).await?,
        };
        out.require_success()?;
        Ok(())
    }
    fn mutation(&self, verb: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(verb);
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        cmd
    }
}

fn reject_pins(packages: &[PackageRequest]) -> Result<()> {
    if let Some(p) = packages.iter().find(|p| p.version.is_some()) {
        Err(Error::Other(format!(
            "{ID}: `{p}` cannot be pinned in classic mode; vcpkg versions are selected through manifests and baselines"
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
        "vcpkg"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "vcpkg"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
    }

    async fn install(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        reject_pins(_packages)?;
        self.run(
            self.mutation("install", _ctx)
                .args(_packages.iter().map(|p| p.name.as_str())),
            _ctx,
        )
        .await
    }

    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        reject_pins(_packages)?;
        self.run(
            self.mutation("remove", _ctx)
                .args(_packages.iter().map(|p| p.name.as_str())),
            _ctx,
        )
        .await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let out = self
            .cmd()
            .args(["list", "--x-full-desc"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_table(&out.stdout, InstallState::Installed)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // `vcpkg list` takes no filter argument; match client-side.
        let installed = parse_table(
            &self
                .cmd()
                .args(["list", "--x-full-desc"])
                .capture(&self.elevator, None)
                .await?
                .require_success()?
                .stdout,
            InstallState::Installed,
        )
        .into_iter()
        .find(|p| base_name(&p.name) == name);
        if let Some(p) = installed {
            return Ok(Box::new(p));
        }
        let out = self
            .cmd()
            .arg("search")
            .arg(name)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        parse_table(&out.stdout, InstallState::Available)
            .into_iter()
            .find(|p| base_name(&p.name) == name)
            .map(|p| Box::new(p) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.into()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let out = self
            .cmd()
            .arg("search")
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_table(&out.stdout, InstallState::Available)))
    }
    /// `vcpkg upgrade [options]` takes no package arguments - it always
    /// rebuilds every outdated classic-mode package - so a targeted
    /// upgrade is not expressible.
    async fn upgrade(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        reject_pins(_packages)?;
        if !_packages.is_empty() {
            return Err(Error::Other(format!(
                "{ID}: vcpkg upgrade takes no package arguments; it can only upgrade the whole installed set"
            )));
        }
        let mut cmd = self.cmd().arg("upgrade");
        if !_ctx.dry_run {
            cmd = cmd.arg("--no-dry-run");
        }
        self.run(cmd, _ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let out = self
            .cmd()
            .arg("upgrade")
            .capture(&self.elevator, None)
            .await?;
        if !out.success() && out.stdout.trim().is_empty() {
            return Err(Error::Other(out.stderr));
        }
        Ok(boxed(parse_upgrade(&out.stdout)))
    }
}

fn base_name(name: &str) -> &str {
    name.split_once(':').map_or(name, |(n, _)| n)
}
fn boxed(v: Vec<VcpkgPackage>) -> Vec<Box<dyn Package>> {
    v.into_iter()
        .map(|p| Box::new(p) as Box<dyn Package>)
        .collect()
}
fn parse_table(stdout: &str, state: InstallState) -> Vec<VcpkgPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim().trim_start_matches('*').trim();
            if line.is_empty()
                || line.starts_with("The following")
                || line.starts_with("Additional packages")
                || line.starts_with("If you")
                || line.starts_with("warning:")
            {
                return None;
            }
            let mut fields = line.split_whitespace();
            let name = fields.next()?;
            let version = fields.next()?;
            if !name.contains(':')
                && !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_[]".contains(c))
            {
                return None;
            }
            Some(VcpkgPackage {
                name: name.into(),
                version: Some(version.trim_end_matches(':').into()),
                description: {
                    let d = fields.collect::<Vec<_>>().join(" ");
                    (!d.is_empty()).then_some(d)
                },
                state,
            })
        })
        .collect()
}
/// `vcpkg upgrade` plan rows:
/// `  * corrade[core,utility]:x64-windows -> 2020.06#5` - one spec token,
/// an arrow, and the target version; no current-version column.
fn parse_upgrade(stdout: &str) -> Vec<VcpkgPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim().trim_start_matches('*').trim();
            let (left, new) = line.split_once(" -> ")?;
            let name = left.trim();
            if name.is_empty() || name.contains(' ') {
                return None;
            }
            Some(VcpkgPackage {
                name: name.into(),
                version: Some(new.split_whitespace().next()?.into()),
                description: None,
                state: InstallState::Upgradable,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_list_and_search() {
        let p = parse_table(
            "fmt:x64-linux 10.2.1 Formatting library for C++\nzlib 1.3.1 Compression\n",
            InstallState::Installed,
        );
        assert_eq!(p.len(), 2);
        assert_eq!(
            p[0].description.as_deref(),
            Some("Formatting library for C++")
        );
    }
    #[test]
    fn parses_upgrade_plan() {
        // Example output from the `vcpkg upgrade` command reference
        // (learn.microsoft.com/en-us/vcpkg/commands/upgrade), abridged.
        let p = parse_upgrade(
            "The following packages will be rebuilt:\n\
             \x20 * corrade[core,interconnect,pluginmanager,testsuite,utility]:x64-windows -> 2020.06#5\n\
             \x20 * openal-soft[core]:x64-windows -> 1.23.0\n\
             \x20 * ragel[core]:x64-windows -> 6.10#5\n\
             Additional packages (*) will be modified to complete this operation.\n\
             If you are sure you want to rebuild the above packages, run this command with the --no-dry-run option.\n",
        );
        assert_eq!(p.len(), 3);
        assert_eq!(
            p[0].name,
            "corrade[core,interconnect,pluginmanager,testsuite,utility]:x64-windows"
        );
        assert_eq!(p[0].version.as_deref(), Some("2020.06#5"));
        assert_eq!(p[1].version.as_deref(), Some("1.23.0"));
        assert_eq!(p[0].state, InstallState::Upgradable);
    }
    #[test]
    fn rejects_classic_pins() {
        assert!(reject_pins(&[PackageRequest::parse("fmt@10.2.1")]).is_err());
    }
}

/// The package type this backend will produce once implemented.
#[derive(Debug)]
pub struct VcpkgPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for VcpkgPackage {
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
