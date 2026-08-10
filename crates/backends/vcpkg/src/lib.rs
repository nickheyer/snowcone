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
        let installed = parse_table(
            &self
                .cmd()
                .args(["list", "--x-full-desc"])
                .arg(name)
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
    async fn upgrade(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        reject_pins(_packages)?;
        let mut cmd = self
            .cmd()
            .arg("upgrade")
            .args(_packages.iter().map(|p| p.name.as_str()));
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
fn parse_upgrade(stdout: &str) -> Vec<VcpkgPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim().trim_start_matches('*').trim();
            let (left, new) = line.split_once(" -> ")?;
            let mut f = left.split_whitespace();
            let name = f.next()?;
            let _old = f.next()?;
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
        let p = parse_upgrade(" * fmt:x64-linux 9.1.0 -> 10.2.1\n");
        assert_eq!(p[0].version.as_deref(), Some("10.2.1"));
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
