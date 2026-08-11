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
        // On install/update `--auto` answers the dependency prompt; on
        // remove it instead also deletes no-longer-needed auto-installed
        // packages, which assume-yes must not opt into.
        if ctx.assume_yes && matches!(verb, "install" | "update") {
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
/// `raco pkg show --all`: per-scope `Installation-wide:`/`User-specific:`
/// headers, a `Package[*=auto]  Checksum  Source` column header (the
/// `[*=auto]` suffix only when an auto-installed package appears, marked by
/// a `*` glued to its name), ` [none]` for empty scopes, and a trailing
/// `[N auto-installed packages not shown]` note outside `--all`.
fn parse_show(s: &str) -> Vec<RacoPackage> {
    s.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.ends_with(':') || l.starts_with('[') {
                return None;
            }
            let mut f = l.split_whitespace();
            let name = f.next()?;
            if name == "Package" || name.starts_with("Package[") {
                return None;
            }
            let name = name.strip_suffix('*').unwrap_or(name);
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
/// `raco pkg catalog-show`: a `Package name: <name>` header, then
/// one-space-indented title-cased fields (` Author:`, ` Source:`,
/// ` Checksum:`, ` Tags:`, ` Description:`, ` Ring:`) for whichever keys
/// the catalog holds, then an optional ` Dependencies:` block.
fn parse_catalog(s: &str) -> Option<RacoPackage> {
    let mut name = None;
    let mut checksum = None;
    let mut description = None;
    for l in s.lines() {
        if let Some(v) = l.strip_prefix("Package name:") {
            name = Some(v.trim().to_owned());
        } else if let Some((k, v)) = l.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "checksum" => checksum = Some(v.trim()).filter(|v| *v != "#f").map(str::to_owned),
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
            "Installation-wide:\n Package[*=auto]      Checksum          Source\n base                 41ea15bc...       (catalog \"base\")\n racket-lib*          d6bf1a2c...       (catalog \"racket-lib\")\nUser-specific:\n [none]\n",
        );
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].name, "base");
        assert_eq!(p[0].version.as_deref(), Some("41ea15bc..."));
        assert_eq!(p[1].name, "racket-lib");
    }
    #[test]
    fn parses_catalog_fields() {
        let p = parse_catalog(
            "Package name: frog\n Author: greg@greghendershott.com\n Source: git://github.com/greghendershott/frog\n Checksum: c30fabd5ba9c15a40699a55b8fed575a4a1cb46f\n Tags: blog\n Description: Frog is a static web site generator written in Racket.\n Ring: 1\n Dependencies:\n  base\n",
        )
        .unwrap();
        assert_eq!(p.name, "frog");
        assert_eq!(
            p.version.as_deref(),
            Some("c30fabd5ba9c15a40699a55b8fed575a4a1cb46f")
        );
        assert_eq!(
            p.description.as_deref(),
            Some("Frog is a static web site generator written in Racket.")
        );
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
