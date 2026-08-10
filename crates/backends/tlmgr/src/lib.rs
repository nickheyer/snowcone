//! TeX Live tlmgr backend for snowcone.
//!
//! TeX Live manager backend using `info --data` and machine-readable updates.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "tlmgr";
const PROGRAMS: &[&str] = &["tlmgr"];

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
        Cmd::new(&self.program).env("LC_ALL", "C")
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
        let mut cmd = self.cmd().arg(verb).elevated(true);
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        cmd
    }
    async fn info_data(&self, args: &[&str]) -> Result<Vec<TlmgrPackage>> {
        let out = self
            .cmd()
            .arg("info")
            .args(args)
            .args([
                "--data",
                "name,localrev,remoterev,cat-version,shortdesc,installed",
            ])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_info_data(&out.stdout))
    }
}

fn reject_pins(packages: &[PackageRequest]) -> Result<()> {
    if let Some(p) = packages.iter().find(|p| p.version.is_some()) {
        Err(Error::Other(format!(
            "{ID}: `{p}` cannot be pinned; TeX Live repositories expose revisions but tlmgr install accepts package names only"
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
        "TeX Live tlmgr"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "texlive"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::REFRESH
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
        Ok(boxed(self.info_data(&["--only-installed"]).await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        self.info_data(&[name])
            .await?
            .into_iter()
            .find(|p| p.name == name)
            .map(|p| Box::new(p) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.into()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let out = self
            .cmd()
            .args(["search", "--global"])
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        let names = parse_search_names(&out.stdout);
        let mut packages = Vec::new();
        for name in names {
            if let Some(p) = self
                .info_data(&[&name])
                .await?
                .into_iter()
                .find(|p| p.name == name)
            {
                packages.push(p);
            }
        }
        Ok(boxed(packages))
    }
    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        let cmd = self.cmd().args(["update", "--list", "--machine-readable"]);
        self.run(cmd, ctx).await
    }

    async fn upgrade(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        reject_pins(_packages)?;
        let mut cmd = self.mutation("update", _ctx);
        if _packages.is_empty() {
            cmd = cmd.arg("--all");
        } else {
            cmd = cmd.args(_packages.iter().map(|p| p.name.as_str()));
        }
        self.run(cmd, _ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let out = self
            .cmd()
            .args(["update", "--list", "--machine-readable"])
            .capture(&self.elevator, None)
            .await?;
        if !out.success() && out.stdout.trim().is_empty() {
            return Err(Error::Other(out.stderr));
        }
        Ok(boxed(parse_updates(&out.stdout)))
    }
}

fn boxed(v: Vec<TlmgrPackage>) -> Vec<Box<dyn Package>> {
    v.into_iter()
        .map(|p| Box::new(p) as Box<dyn Package>)
        .collect()
}
fn parse_info_data(stdout: &str) -> Vec<TlmgrPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 6 {
                return None;
            }
            let name = f[0].trim();
            if name.is_empty() || name == "name" {
                return None;
            }
            let local = f[1].trim();
            let remote = f[2].trim();
            let catalog = f[3].trim();
            let installed = matches!(f[5].trim(), "1" | "true" | "yes");
            let state = if installed && local != "-" && remote != "-" && local != remote {
                InstallState::Upgradable
            } else if installed {
                InstallState::Installed
            } else {
                InstallState::Available
            };
            let version = if !catalog.is_empty() && catalog != "-" {
                Some(catalog.into())
            } else if installed && local != "-" {
                Some(local.into())
            } else if remote != "-" {
                Some(remote.into())
            } else {
                None
            };
            Some(TlmgrPackage {
                name: name.into(),
                version,
                description: (!f[4].trim().is_empty()).then(|| f[4].trim().into()),
                state,
            })
        })
        .collect()
}
fn parse_search_names(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with(' ') || line.starts_with("tlmgr:") {
                return None;
            }
            line.strip_suffix(':')
                .filter(|n| !n.contains(' '))
                .map(str::to_owned)
        })
        .collect()
}
fn parse_updates(stdout: &str) -> Vec<TlmgrPackage> {
    let mut body = false;
    stdout
        .lines()
        .filter_map(|line| {
            if line == "end-of-header" {
                body = true;
                return None;
            }
            if line == "end-of-updates" {
                body = false;
                return None;
            }
            if !body {
                return None;
            }
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 4 || !matches!(f[1], "u" | "a" | "i" | "I") {
                return None;
            }
            Some(TlmgrPackage {
                name: f[0].into(),
                version: (f[3] != "-").then(|| f[3].into()),
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
    fn parses_data_rows() {
        let p = parse_info_data(
            "fontspec\t70000\t71000\t2.9g\tAdvanced font selection\t1\nlatexmk\t-\t72000\t4.86a\tAutomation\t0\n",
        );
        assert_eq!(p[0].state, InstallState::Upgradable);
        assert_eq!(p[1].state, InstallState::Available);
    }
    #[test]
    fn parses_machine_updates() {
        let p = parse_updates(
            "location-url\thttps://example\nend-of-header\nfontspec u 70000 71000 10 0 0\nold d 1 - 0 0 0\nend-of-updates\n",
        );
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].version.as_deref(), Some("71000"));
    }
    #[test]
    fn finds_search_headers() {
        assert_eq!(
            parse_search_names("fontspec:\n\ttexmf-dist/foo\nlatex-fontspec:\n"),
            ["fontspec", "latex-fontspec"]
        );
    }
}

/// The package type this backend will produce once implemented.
#[derive(Debug)]
pub struct TlmgrPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for TlmgrPackage {
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
