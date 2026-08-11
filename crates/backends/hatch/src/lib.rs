//! Hatch backend for snowcone.
//!
//! Hatch is a project manager with no CLI verbs for installing packages -
//! dependencies are edited in pyproject.toml by hand, and `hatch env` is
//! project-scoped machinery. Its one real, user-level install surface is
//! `hatch python`: management of standalone Python distributions
//! (`hatch python install 3.12`). That is what this backend drives, so
//! "packages" here are distribution names like `3.12` or `pypy3.10`, not
//! PyPI projects. `hatch python update NAMES...` upgrades installed
//! distributions in place, with `all` standing in for the whole set.
//! `hatch python show` renders rich tables, so reads run with NO_COLOR
//! under LC_ALL=C and the parser strips the box drawing. No `hatch python`
//! verb has a dry-run, so `dry_run` errors.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "hatch";
const PROGRAMS: &[&str] = &["hatch"];

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
            .find_map(|program| find_program(program))
            .ok_or_else(|| Error::Unavailable(ID.to_string()))?;
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

    /// Read invocation with a stable locale and rich's coloring off, so
    /// the table output parses cleanly.
    fn query(&self) -> Cmd {
        Cmd::new(&self.program)
            .env("LC_ALL", "C")
            .env("NO_COLOR", "1")
    }

    /// CLI passthrough when no event consumer is attached, captured and
    /// streamed otherwise.
    async fn run(&self, cmd: Cmd, ctx: &OpContext) -> Result<()> {
        let output = match &ctx.events {
            Some(events) => cmd.capture(&self.elevator, Some(events)).await?,
            None => cmd.run_interactive(&self.elevator).await?,
        };
        output.require_success()?;
        Ok(())
    }

    fn no_dry_run(&self, operation: &str) -> Error {
        Error::Other(format!("{ID}: {operation} has no dry-run mode"))
    }

    /// Installed and available distributions, from `hatch python show`.
    async fn show(&self) -> Result<Vec<HatchPackage>> {
        let output = self
            .query()
            .args(["python", "show"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_python_show(&output.stdout))
    }
}

/// Distribution names label a fixed upstream build; there is no version
/// to choose at install time.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but `hatch python` always installs the current build of a distribution"
        ))),
        None => Ok(()),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Hatch"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "python"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::UPGRADE
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        for package in packages {
            self.run(
                self.cmd().args(["python", "install"]).arg(&package.name),
                ctx,
            )
            .await?;
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        for package in packages {
            self.run(
                self.cmd().args(["python", "remove"]).arg(&package.name),
                ctx,
            )
            .await?;
        }
        Ok(())
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .show()
            .await?
            .into_iter()
            .filter(|package| package.state == InstallState::Installed)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let mut pythons = self.show().await?;
        let index = pythons
            .iter()
            .position(|python| python.name == name && python.state == InstallState::Installed)
            .or_else(|| pythons.iter().position(|python| python.name == name))
            .ok_or_else(|| Error::NotFound(name.to_string()))?;
        Ok(Box::new(pythons.swap_remove(index)))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        // `hatch python update NAMES...` takes multiple names in one
        // invocation; the literal name `all` covers every installed
        // distribution.
        let mut cmd = self.cmd().args(["python", "update"]);
        if packages.is_empty() {
            cmd = cmd.arg("all");
        } else {
            cmd = cmd.args(packages.iter().map(|package| package.name.as_str()));
        }
        self.run(cmd, ctx).await
    }
}

/// `hatch python show`: rich tables titled `Installed` and `Available`
/// with Name/Version columns; data rows are `│`-separated cells, and
/// border/header rows fail the version check.
fn parse_python_show(stdout: &str) -> Vec<HatchPackage> {
    let mut packages = Vec::new();
    let mut state = InstallState::Unknown;
    for line in stdout.lines() {
        let trimmed = line.trim();
        match trimmed {
            "Installed" => {
                state = InstallState::Installed;
                continue;
            }
            "Available" => {
                state = InstallState::Available;
                continue;
            }
            _ => {}
        }
        let cells: Vec<&str> = trimmed
            .split(['│', '┃', '|'])
            .map(str::trim)
            .filter(|cell| !cell.is_empty())
            .collect();
        let [name, version, ..] = cells[..] else {
            continue;
        };
        if state == InstallState::Unknown || !version.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        packages.push(HatchPackage {
            name: name.to_string(),
            version: Some(version.to_string()),
            state,
        });
    }
    packages
}

/// A Python distribution as `hatch python show` describes it.
#[derive(Debug, Default)]
pub struct HatchPackage {
    pub name: String,
    pub version: Option<String>,
    pub state: InstallState,
}

impl Package for HatchPackage {
    fn manager(&self) -> &str {
        ID
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_show_tables() {
        let stdout = "\
     Installed
┏━━━━━━┳━━━━━━━━━┓
┃ Name ┃ Version ┃
┡━━━━━━╇━━━━━━━━━┩
│ 3.12 │ 3.12.3  │
└──────┴─────────┘
     Available
┏━━━━━━━━━━┳━━━━━━━━━┓
┃ Name     ┃ Version ┃
┡━━━━━━━━━━╇━━━━━━━━━┩
│ 3.11     │ 3.11.9  │
│ pypy3.10 │ 7.3.15  │
└──────────┴─────────┘
";
        let packages = parse_python_show(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "3.12");
        assert_eq!(packages[0].version.as_deref(), Some("3.12.3"));
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[1].state, InstallState::Available);
        assert_eq!(packages[2].name, "pypy3.10");
        assert_eq!(packages[2].version.as_deref(), Some("7.3.15"));
    }

    #[test]
    fn parses_ascii_rows() {
        let stdout = "\
Installed
| 3.12 | 3.12.3 |
";
        let packages = parse_python_show(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "3.12");
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn rows_outside_a_section_are_ignored() {
        assert!(parse_python_show("│ 3.12 │ 3.12.3 │\n").is_empty());
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("3.12@3.12.3")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("3.12")]).is_ok());
    }
}
