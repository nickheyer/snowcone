//! Coursier backend for snowcone.
//!
//! Drives coursier's application channel (`cs install` / `cs uninstall` /
//! `cs update` / `cs list`), which manages launchers under coursier's own
//! bin directory. `cs list` prints bare app names, one per line, and
//! coursier has no app metadata verb - so listings and info() carry no
//! versions. coursier never prompts, so `assume_yes` has nothing to do,
//! and none of the app commands document a dry-run flag. Search maps onto
//! `cs complete-dep`, which completes Maven coordinates from the
//! repositories' directory listings.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "coursier";
const PROGRAMS: &[&str] = &["cs", "coursier"];

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
                reason: format!("none of {PROGRAMS:?} found on PATH"),
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
    /// Mutating invocation, in the user's locale (output is passed through).
    fn cmd(&self) -> Cmd {
        Cmd::new(&self.program)
    }

    /// Read invocation with a stable locale, so parsing survives i18n.
    fn query(&self) -> Cmd {
        Cmd::new(&self.program).env("LC_ALL", "C")
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

    async fn installed(&self) -> Result<Vec<CoursierPackage>> {
        let output = self
            .query()
            .arg("list")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_list(&output.stdout))
    }
}

/// `name:version` when the request pins one - coursier's app-descriptor
/// version syntax - bare name otherwise.
fn spec(request: &PackageRequest) -> String {
    match &request.version {
        Some(version) => format!("{}:{version}", request.name),
        None => request.name.clone(),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Coursier"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "jvm"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::UPGRADE
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        let cmd = self.cmd().arg("install").args(packages.iter().map(spec));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("uninstall"));
        }
        let cmd = self
            .cmd()
            .arg("uninstall")
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .installed()
            .await?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // coursier has no app metadata verb; the installed list is all
        // there is to report.
        self.installed()
            .await?
            .into_iter()
            .find(|package| package.name == name)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("complete-dep")
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_completions(&output.stdout)
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        if packages.is_empty() {
            return self.run(self.cmd().arg("update"), ctx).await;
        }
        // `cs update` takes bare app names; a pinned target switches
        // version through `cs install name:version` instead.
        let unpinned: Vec<&str> = packages
            .iter()
            .filter(|package| package.version.is_none())
            .map(|package| package.name.as_str())
            .collect();
        if !unpinned.is_empty() {
            self.run(self.cmd().arg("update").args(unpinned), ctx)
                .await?;
        }
        let pinned: Vec<String> = packages
            .iter()
            .filter(|package| package.version.is_some())
            .map(spec)
            .collect();
        if !pinned.is_empty() {
            self.run(self.cmd().arg("install").args(pinned), ctx)
                .await?;
        }
        Ok(())
    }
}

/// `cs list`: one installed application name per line, nothing else.
fn parse_list(stdout: &str) -> Vec<CoursierPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let name = line.trim();
            (!name.is_empty()).then(|| CoursierPackage {
                name: name.to_string(),
                state: InstallState::Installed,
            })
        })
        .collect()
}

/// `cs complete-dep`: one Maven-coordinate completion per line - orgs for
/// a bare prefix, artifact names after `org:`, versions after
/// `org:artifact:`.
fn parse_completions(stdout: &str) -> Vec<CoursierPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let name = line.trim();
            (!name.is_empty()).then(|| CoursierPackage {
                name: name.to_string(),
                state: InstallState::Available,
            })
        })
        .collect()
}

/// An application as coursier describes it - `cs list` yields bare names
/// only, so there is no version or description to carry.
#[derive(Debug)]
pub struct CoursierPackage {
    pub name: String,
    pub state: InstallState,
}

impl Package for CoursierPackage {
    fn manager(&self) -> &str {
        ID
    }

    fn name(&self) -> &str {
        &self.name
    }

    /// `cs list` never reports versions, so there is none to return.
    fn version(&self) -> Option<&str> {
        None
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_installed_apps() {
        let stdout = "ammonite\nbloop\nscalafmt\n\n";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "ammonite");
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn parses_dependency_completions() {
        let stdout = "io.circe\nio.circe.optics\n";
        let packages = parse_completions(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "io.circe.optics");
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("scalafmt@3.8.1")),
            "scalafmt:3.8.1"
        );
        assert_eq!(spec(&PackageRequest::parse("scalafmt")), "scalafmt");
    }
}
