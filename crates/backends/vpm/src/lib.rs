//! V vpm backend for snowcone.
//!
//! V's package commands install modules into VMODULES (normally ~/.vmodules).

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "vpm";
const PROGRAMS: &[&str] = &["v"];

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
    async fn run(&self, cmd: Cmd, ctx: &OpContext) -> Result<()> {
        let out = match &ctx.events {
            Some(events) => cmd.capture(&self.elevator, Some(events)).await?,
            None => cmd.run_interactive(&self.elevator).await?,
        };
        out.require_success()?;
        Ok(())
    }
    fn mutation(&self, verb: &str, ctx: &OpContext) -> Result<Cmd> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: {verb} has no dry-run mode")));
        }
        Ok(self.cmd().arg(verb))
    }
}

fn reject_pins(packages: &[PackageRequest]) -> Result<()> {
    if let Some(package) = packages.iter().find(|p| p.version.is_some()) {
        return Err(Error::Other(format!(
            "{ID}: `{package}` cannot be pinned; VPM package commands do not accept versions"
        )));
    }
    Ok(())
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "V vpm"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "vpm"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::SEARCH | Capabilities::UPGRADE
    }

    async fn install(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        reject_pins(_packages)?;
        self.run(
            self.mutation("install", _ctx)?
                .args(_packages.iter().map(|p| p.name.as_str())),
            _ctx,
        )
        .await
    }

    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        reject_pins(_packages)?;
        self.run(
            self.mutation("remove", _ctx)?
                .args(_packages.iter().map(|p| p.name.as_str())),
            _ctx,
        )
        .await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let out = self
            .cmd()
            .arg("list")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_list(&out.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let installed = parse_list(
            &self
                .cmd()
                .arg("list")
                .capture(&self.elevator, None)
                .await?
                .require_success()?
                .stdout,
        )
        .into_iter()
        .any(|p| p.name == name);
        let out = self
            .cmd()
            .arg("search")
            .arg(name)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        let mut package = parse_search(&out.stdout)
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| Error::NotFound(name.into()))?;
        if installed {
            package.state = InstallState::Installed;
        }
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let out = self
            .cmd()
            .arg("search")
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_search(&out.stdout)))
    }

    async fn upgrade(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        reject_pins(_packages)?;
        let verb = if _packages.is_empty() {
            "upgrade"
        } else {
            "update"
        };
        self.run(
            self.mutation(verb, _ctx)?
                .args(_packages.iter().map(|p| p.name.as_str())),
            _ctx,
        )
        .await
    }
}

fn boxed(v: Vec<VpmPackage>) -> Vec<Box<dyn Package>> {
    v.into_iter()
        .map(|p| Box::new(p) as Box<dyn Package>)
        .collect()
}
fn parse_list(stdout: &str) -> Vec<VpmPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let name = line.trim().trim_start_matches(['-', '*', ' ']).trim();
            if name.is_empty() || name.eq_ignore_ascii_case("Installed packages:") {
                None
            } else {
                Some(VpmPackage {
                    name: name.into(),
                    version: None,
                    description: None,
                    state: InstallState::Installed,
                })
            }
        })
        .collect()
}
fn parse_search(stdout: &str) -> Vec<VpmPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim().trim_start_matches(['-', '*', ' ']).trim();
            if line.is_empty()
                || line.starts_with("Search results")
                || line.starts_with("No module")
            {
                return None;
            }
            let (name, description) = line
                .split_once(" - ")
                .or_else(|| line.split_once('\t'))
                .map_or((line, None), |(n, d)| (n, Some(d.trim().into())));
            Some(VpmPackage {
                name: name.trim().into(),
                version: None,
                description,
                state: InstallState::Available,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_installed_names() {
        let p = parse_list("Installed packages:\n  markdown\n  ui\n");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].name, "markdown");
    }
    #[test]
    fn parses_search_descriptions() {
        let p = parse_search("markdown - Markdown parser\nui\tCross-platform UI\n");
        assert_eq!(p[1].description.as_deref(), Some("Cross-platform UI"));
    }
    #[test]
    fn rejects_pins() {
        assert!(reject_pins(&[PackageRequest::parse("ui@1.0")]).is_err());
    }
}

/// The package type this backend will produce once implemented.
#[derive(Debug)]
pub struct VpmPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for VpmPackage {
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
