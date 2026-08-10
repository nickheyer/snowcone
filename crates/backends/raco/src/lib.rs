//! Racket raco pkg backend for snowcone.
//!
//! Racket package manager backend using `raco pkg` commands.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "raco";
const PROGRAMS: &[&str] = &["raco"];

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
        Cmd::new(&self.program).args(["pkg"])
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
        let mut c = self.cmd().arg(verb);
        if ctx.assume_yes {
            c = c.arg("--auto");
        }
        if ctx.dry_run {
            c = c.arg("--dry-run");
        }
        c
    }
    fn reject_pins(&self, packages: &[PackageRequest]) -> Result<()> {
        if let Some(p) = packages.iter().find(|p| p.version.is_some()) {
            Err(Error::Other(format!(
                "{ID}: `{p}` cannot be version-pinned; Racket catalogs identify releases by checksum"
            )))
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Racket raco pkg"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "racket"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::UPGRADE
    }

    async fn install(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        self.reject_pins(_packages)?;
        self.run(
            self.mutation("install", _ctx)
                .args(_packages.iter().map(|p| p.name.as_str())),
            _ctx,
        )
        .await
    }

    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        self.reject_pins(_packages)?;
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
            .args(["show", "--all"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_show(&out.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        if let Some(p) = parse_show(
            &self
                .cmd()
                .args(["show", "--all"])
                .arg(name)
                .capture(&self.elevator, None)
                .await?
                .stdout,
        )
        .into_iter()
        .find(|p| p.name == name)
        {
            return Ok(Box::new(p));
        }
        let out = self
            .cmd()
            .arg("catalog-show")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !out.success() {
            return Err(Error::NotFound(name.into()));
        }
        parse_catalog(&out.stdout)
            .map(|p| Box::new(p) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.into()))
    }

    async fn upgrade(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        self.reject_pins(_packages)?;
        let mut c = self.mutation("update", _ctx);
        if _packages.is_empty() {
            c = c.arg("--all");
        } else {
            c = c.args(_packages.iter().map(|p| p.name.as_str()));
        }
        self.run(c, _ctx).await
    }
}

fn boxed(v: Vec<RacoPackage>) -> Vec<Box<dyn Package>> {
    v.into_iter()
        .map(|p| Box::new(p) as Box<dyn Package>)
        .collect()
}
fn parse_show(s: &str) -> Vec<RacoPackage> {
    s.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.ends_with(':') || l.starts_with("Package ") || l == "[none]" {
                return None;
            }
            let mut f = l.split_whitespace();
            let name = f.next()?;
            let checksum = f.next().filter(|v| *v != "#f").map(str::to_owned);
            Some(RacoPackage {
                name: name.into(),
                version: checksum,
                description: None,
                state: InstallState::Installed,
            })
        })
        .collect()
}
fn parse_catalog(s: &str) -> Option<RacoPackage> {
    let mut name = None;
    let mut checksum = None;
    let mut description = None;
    for l in s.lines() {
        if let Some((k, v)) = l.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "name" => name = Some(v.trim().into()),
                "checksum" => checksum = Some(v.trim().into()),
                "description" => description = Some(v.trim().into()),
                _ => {}
            }
        }
    }
    name.map(|name| RacoPackage {
        name,
        version: checksum,
        description,
        state: InstallState::Available,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_scoped_show() {
        let p = parse_show(
            "Installation-wide:\n Package Checksum Source\n racket-lib abc123 (catalog racket-lib)\nUser-specific:\n [none]\n",
        );
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].name, "racket-lib");
        assert_eq!(p[0].version.as_deref(), Some("abc123"));
    }
    #[test]
    fn parses_catalog_fields() {
        let p = parse_catalog("name: frog\nchecksum: deadbeef\ndescription: Frog tools\n").unwrap();
        assert_eq!(p.description.as_deref(), Some("Frog tools"));
    }
}

/// The package type this backend will produce once implemented.
#[derive(Debug)]
pub struct RacoPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for RacoPackage {
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
