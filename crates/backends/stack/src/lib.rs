//! Stack backend for snowcone.
//!
//! What Stack can honestly do globally: `stack install <target>` (a
//! synonym for `stack build --copy-bins`) builds a package and copies its
//! executables into the local bin directory, and works outside a project
//! through the implicit global project; `stack update` refreshes the
//! Hackage package index; `stack list <pkg>` reports the latest Hackage
//! version of a package. There is no global remove (copied executables
//! are unrecorded files), no global installed list, no search, and no
//! upgrade verb for installed executables (`stack upgrade` upgrades Stack
//! itself) - those stay unadvertised. `stack ls dependencies` is
//! project-scoped, so it is not used here.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "stack";
const PROGRAMS: &[&str] = &["stack"];

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
}

/// `name-version` when the request pins one - Stack's documented target
/// syntax for a package-index version - bare name otherwise.
fn spec(request: &PackageRequest) -> String {
    match &request.version {
        Some(version) => format!("{}-{version}", request.name),
        None => request.name.clone(),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Stack"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "haskell"
    }

    /// No REMOVE (copied executables are unrecorded files), no
    /// LIST_INSTALLED (nothing records what was copy-installed), no
    /// UPGRADE (`stack upgrade` upgrades Stack itself, not packages).
    fn capabilities(&self) -> Capabilities {
        Capabilities::INSTALL
            | Capabilities::INFO
            | Capabilities::REFRESH
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let mut cmd = self.cmd().arg("install");
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        self.run(cmd.args(packages.iter().map(spec)), ctx).await
    }

    /// Stack records nothing about copy-installed executables; removal is
    /// deleting the file from the local bin directory by hand.
    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        Err(self.unsupported(Operation::Remove))
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Err(self.unsupported(Operation::ListInstalled))
    }

    /// `stack list <pkg>` prints the latest Hackage version as
    /// `name-version`, or fails when the package does not exist.
    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query()
            .arg("list")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        parse_list(&output.stdout)
            .into_iter()
            .find(|package| package.name == name)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: refresh has no dry-run mode")));
        }
        self.run(self.cmd().arg("update"), ctx).await
    }
}

/// `stack list`: one `name-version` line per package. Hackage version
/// suffixes are all digits and dots, and name segments always contain a
/// letter, so the version starts after the last dash whose remainder is
/// purely numeric.
fn parse_list(stdout: &str) -> Vec<StackPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.contains(' ') {
                return None;
            }
            let (name, version) = match line.rsplit_once('-') {
                Some((name, version))
                    if !version.is_empty()
                        && version.chars().all(|c| c.is_ascii_digit() || c == '.') =>
                {
                    (name, Some(version))
                }
                _ => (line, None),
            };
            if name.is_empty() {
                return None;
            }
            Some(StackPackage {
                name: name.to_string(),
                version: version.map(str::to_string),
                description: None,
                // The latest Hackage version; whether one of its
                // executables was ever copy-installed is unrecorded.
                state: InstallState::Available,
            })
        })
        .collect()
}

/// A package as Stack describes it.
#[derive(Debug)]
pub struct StackPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for StackPackage {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stack_list_output() {
        // Example from the `stack list` documentation
        // (docs.haskellstack.org/en/stable/commands/list_command).
        let packages = parse_list("base-4.21.0.0\nunix-2.8.6.0\nWin32-2.14.1.0\n");
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "base");
        assert_eq!(packages[0].version.as_deref(), Some("4.21.0.0"));
        assert_eq!(packages[2].name, "Win32");
        assert_eq!(packages[2].version.as_deref(), Some("2.14.1.0"));
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn splits_hyphenated_package_names() {
        let packages = parse_list("optparse-applicative-0.18.1.0\n");
        assert_eq!(packages[0].name, "optparse-applicative");
        assert_eq!(packages[0].version.as_deref(), Some("0.18.1.0"));
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(spec(&PackageRequest::parse("hlint@3.8")), "hlint-3.8");
        assert_eq!(spec(&PackageRequest::parse("hlint")), "hlint");
    }
}
