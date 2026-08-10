//! MiKTeX mpm backend for snowcone.
//!
//! Supports both the current `miktex packages` frontend and legacy `mpm`.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "mpm";
const PROGRAMS: &[&str] = &["miktex", "mpm"];

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
        let program = PROGRAMS
            .iter()
            .find_map(|p| find_program(p))
            .ok_or_else(|| Error::Unavailable(ID.into()))?;
        let modern = program.file_stem().is_some_and(|p| p == "miktex");
        Ok(Box::new(Manager {
            program,
            elevator: Elevator::detect(host),
            modern,
        }))
    }
}

struct Manager {
    program: PathBuf,
    elevator: Elevator,
    modern: bool,
}

impl Manager {
    fn cmd(&self) -> Cmd {
        let cmd = Cmd::new(&self.program).env("LC_ALL", "C");
        if self.modern {
            cmd.arg("packages")
        } else {
            cmd
        }
    }
    async fn run(&self, cmd: Cmd, ctx: &OpContext) -> Result<()> {
        let out = match &ctx.events {
            Some(e) => cmd.capture(&self.elevator, Some(e)).await?,
            None => cmd.run_interactive(&self.elevator).await?,
        };
        out.require_success()?;
        Ok(())
    }
    fn no_dry(&self, ctx: &OpContext, op: &str) -> Result<()> {
        if ctx.dry_run {
            Err(Error::Other(format!("{ID}: {op} has no dry-run mode")))
        } else {
            Ok(())
        }
    }
    async fn modern_list(&self) -> Result<Vec<MpmPackage>> {
        let template = "{id}\t{version}\t{description}\t{isInstalled}\n";
        let out = self
            .cmd()
            .args(["list", "--template", template])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_template(&out.stdout))
    }
}

fn reject_pins(packages: &[PackageRequest]) -> Result<()> {
    if let Some(p) = packages.iter().find(|p| p.version.is_some()) {
        Err(Error::Other(format!(
            "{ID}: `{p}` cannot be pinned; MiKTeX selects repository package versions"
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
        "MiKTeX mpm"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "miktex"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::SEARCH | Capabilities::REFRESH | Capabilities::UPGRADE
    }

    async fn install(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        reject_pins(_packages)?;
        self.no_dry(_ctx, "install")?;
        if self.modern {
            self.run(
                self.cmd()
                    .arg("install")
                    .args(_packages.iter().map(|p| p.name.as_str())),
                _ctx,
            )
            .await
        } else {
            for p in _packages {
                self.run(self.cmd().arg(format!("--install={}", p.name)), _ctx)
                    .await?;
            }
            Ok(())
        }
    }

    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        reject_pins(_packages)?;
        self.no_dry(_ctx, "remove")?;
        if self.modern {
            self.run(
                self.cmd()
                    .arg("remove")
                    .args(_packages.iter().map(|p| p.name.as_str())),
                _ctx,
            )
            .await
        } else {
            for p in _packages {
                self.run(self.cmd().arg(format!("--uninstall={}", p.name)), _ctx)
                    .await?;
            }
            Ok(())
        }
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let packages = if self.modern {
            self.modern_list().await?
        } else {
            let out = self
                .cmd()
                .arg("--list")
                .capture(&self.elevator, None)
                .await?
                .require_success()?;
            parse_legacy(&out.stdout)
        };
        Ok(boxed(
            packages
                .into_iter()
                .filter(|p| p.state == InstallState::Installed)
                .collect(),
        ))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let packages = if self.modern {
            let t = "{id}\t{version}\t{description}\t{isInstalled}\n";
            let out = self
                .cmd()
                .args(["info", "--template", t])
                .arg(name)
                .capture(&self.elevator, None)
                .await?;
            if !out.success() {
                return Err(Error::NotFound(name.into()));
            }
            parse_template(&out.stdout)
        } else {
            parse_legacy(
                &self
                    .cmd()
                    .arg("--list")
                    .capture(&self.elevator, None)
                    .await?
                    .require_success()?
                    .stdout,
            )
        };
        packages
            .into_iter()
            .find(|p| p.name == name)
            .map(|p| Box::new(p) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.into()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let all = if self.modern {
            self.modern_list().await?
        } else {
            parse_legacy(
                &self
                    .cmd()
                    .arg("--list")
                    .capture(&self.elevator, None)
                    .await?
                    .require_success()?
                    .stdout,
            )
        };
        let q = query.to_ascii_lowercase();
        Ok(boxed(
            all.into_iter()
                .filter(|p| {
                    p.name.to_ascii_lowercase().contains(&q)
                        || p.description
                            .as_deref()
                            .is_some_and(|d| d.to_ascii_lowercase().contains(&q))
                })
                .collect(),
        ))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        self.no_dry(ctx, "refresh")?;
        let cmd = if self.modern {
            self.cmd().arg("update-package-database")
        } else {
            self.cmd().arg("--update-db")
        };
        self.run(cmd, ctx).await
    }

    async fn upgrade(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        reject_pins(_packages)?;
        self.no_dry(_ctx, "upgrade")?;
        if self.modern {
            self.run(
                self.cmd()
                    .arg("update")
                    .args(_packages.iter().map(|p| p.name.as_str())),
                _ctx,
            )
            .await
        } else if _packages.is_empty() {
            self.run(self.cmd().arg("--update"), _ctx).await
        } else {
            for p in _packages {
                self.run(self.cmd().arg(format!("--update={}", p.name)), _ctx)
                    .await?;
            }
            Ok(())
        }
    }
}

fn boxed(v: Vec<MpmPackage>) -> Vec<Box<dyn Package>> {
    v.into_iter()
        .map(|p| Box::new(p) as Box<dyn Package>)
        .collect()
}
fn truthy(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "yes" | "1")
}
fn parse_template(stdout: &str) -> Vec<MpmPackage> {
    stdout
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(4, '\t');
            let name = f.next()?.trim();
            if name.is_empty() {
                return None;
            }
            let version = f
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let description = f
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let state = if truthy(f.next().unwrap_or("")) {
                InstallState::Installed
            } else {
                InstallState::Available
            };
            Some(MpmPackage {
                name: name.into(),
                version,
                description,
                state,
            })
        })
        .collect()
}
fn parse_legacy(stdout: &str) -> Vec<MpmPackage> {
    stdout
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let marker = f.next()?;
            let installed = matches!(marker, "i" | "I" | "installed");
            let name = if installed || matches!(marker, "-" | "u") {
                f.next()?
            } else {
                marker
            };
            if name.starts_with("mpm:") {
                return None;
            }
            let version = f.next().map(str::to_owned);
            Some(MpmPackage {
                name: name.into(),
                version,
                description: None,
                state: if installed {
                    InstallState::Installed
                } else {
                    InstallState::Available
                },
            })
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_modern_template() {
        let p = parse_template("amsmath\t2.17y\tAMS math\ttrue\nfoo\t1.0\tOther\tfalse\n");
        assert_eq!(p[0].state, InstallState::Installed);
        assert_eq!(p[1].state, InstallState::Available);
    }
    #[test]
    fn parses_legacy_rows() {
        let p = parse_legacy("i amsmath 2.17y\n- foo 1.0\n");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].name, "amsmath");
    }
    #[test]
    fn rejects_pins() {
        assert!(reject_pins(&[PackageRequest::parse("amsmath@2.0")]).is_err());
    }
}

/// The package type this backend will produce once implemented.
#[derive(Debug)]
pub struct MpmPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for MpmPackage {
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
