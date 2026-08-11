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
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
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

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let out = self
            .cmd()
            .arg("outdated")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_outdated(&out.stdout)))
    }
}

fn boxed(v: Vec<VpmPackage>) -> Vec<Box<dyn Package>> {
    v.into_iter()
        .map(|p| Box::new(p) as Box<dyn Package>)
        .collect()
}
/// `v list`: one installed module id (`author.module`) per line, or the
/// sentence `You have no modules installed.` - module ids never contain
/// spaces, so sentence lines are dropped wholesale.
fn parse_list(stdout: &str) -> Vec<VpmPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let name = line.trim();
            if name.is_empty() || name.contains(' ') {
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
/// `v search`: after a `Search results for `…`:` header, hits print as
/// `1. markdown by pisaiah [pisaiah.markdown] (installed)` - the
/// installable module id is the bracketed token; ` by author` and
/// ` (installed)` are optional. Misses print `No module(s) found …`.
fn parse_search(stdout: &str) -> Vec<VpmPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let (_, rest) = line.split_once('[')?;
            let (name, rest) = rest.split_once(']')?;
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            Some(VpmPackage {
                name: name.into(),
                version: None,
                description: None,
                state: if rest.contains("(installed)") {
                    InstallState::Installed
                } else {
                    InstallState::Available
                },
            })
        })
        .collect()
}
/// `v outdated`: an `Outdated modules:` header with one indented module id
/// per line, or `Modules are up to date.` / `No modules installed.` when
/// there is nothing to report.
fn parse_outdated(stdout: &str) -> Vec<VpmPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let name = line.trim();
            if name.is_empty() || name.ends_with(':') || name.contains(' ') {
                return None;
            }
            Some(VpmPackage {
                name: name.into(),
                version: None,
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
    fn parses_installed_names() {
        // `v list` prints bare module ids (vlang cmd/tools/vpm/vpm.v),
        // or a sentence when nothing is installed.
        let p = parse_list("pisaiah.markdown\nui\n");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].name, "pisaiah.markdown");
        assert!(parse_list("You have no modules installed.\n").is_empty());
    }
    #[test]
    fn parses_search_hits() {
        // Format from vlang cmd/tools/vpm/search.v:
        // `${index}. ${name}${author}[${mod}]${installed}`.
        let p = parse_search(
            "Search results for `markdown`:\n\n\
             1. markdown by pisaiah [pisaiah.markdown] (installed)\n\
             2. vmarkdown [vmarkdown]\n",
        );
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].name, "pisaiah.markdown");
        assert_eq!(p[0].state, InstallState::Installed);
        assert_eq!(p[1].name, "vmarkdown");
        assert_eq!(p[1].state, InstallState::Available);
        assert!(parse_search("No module(s) found for `nope` .\n").is_empty());
    }
    #[test]
    fn parses_outdated_modules() {
        // Format from vlang cmd/tools/vpm/outdated.v.
        let p = parse_outdated("Outdated modules:\n  pisaiah.markdown\n  ui\n");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].name, "pisaiah.markdown");
        assert_eq!(p[0].state, InstallState::Upgradable);
        assert!(parse_outdated("Modules are up to date.\n").is_empty());
        assert!(parse_outdated("No modules installed.\n").is_empty());
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
