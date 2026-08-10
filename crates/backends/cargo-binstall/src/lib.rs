//! cargo-binstall backend for snowcone.
//!
//! Installs prebuilt binaries for crates.io packages and records them in
//! cargo's own install registry ($CARGO_HOME/.crates.toml). binstall has no
//! verbs of its own for anything but installing, so the honest listing and
//! uninstall go through the sibling `cargo` binary (`cargo install --list`
//! / `cargo uninstall`) and error when `cargo` is missing - a binstall-only
//! host can be. Install has a native `--dry-run`; `--no-confirm` answers
//! its confirmation prompt when `assume_yes` is set.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "cargo-binstall";
const PROGRAMS: &[&str] = &["cargo-binstall"];

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

    /// The sibling `cargo` binary: binstall writes cargo's install registry,
    /// so listing and uninstalling truthfully belong to cargo itself.
    fn cargo(&self) -> Result<PathBuf> {
        find_program("cargo").ok_or_else(|| {
            Error::Other(format!(
                "{ID}: `cargo` is required to list and uninstall binstall-managed crates"
            ))
        })
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

    async fn installed(&self) -> Result<Vec<CargoBinstallPackage>> {
        let output = Cmd::new(self.cargo()?)
            .env("LC_ALL", "C")
            .args(["install", "--list"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_install_list(&output.stdout))
    }
}

/// `name@version` when the request pins one, bare name otherwise.
fn spec(request: &PackageRequest) -> String {
    match &request.version {
        Some(version) => format!("{}@{version}", request.name),
        None => request.name.clone(),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "cargo-binstall"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "cargo"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let mut cmd = self.cmd();
        if ctx.assume_yes {
            cmd = cmd.arg("--no-confirm");
        }
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        self.run(cmd.args(packages.iter().map(spec)), ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: remove has no dry-run mode")));
        }
        let cmd = Cmd::new(self.cargo()?)
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
        // binstall has no metadata verb, so info only covers what the
        // shared install registry knows: locally installed crates.
        self.installed()
            .await?
            .into_iter()
            .find(|package| package.name == name)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }
}

/// `cargo install --list`: unindented `name vX.Y.Z[ (source)]:` headers with
/// the crate's binaries indented below; registry installs carry no source.
fn parse_install_list(stdout: &str) -> Vec<CargoBinstallPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            if line.starts_with(char::is_whitespace) {
                return None;
            }
            let header = line.strip_suffix(':')?;
            let mut parts = header.split_whitespace();
            let name = parts.next()?;
            let version = parts.next()?.strip_prefix('v')?;
            let origin = header
                .split_once(" (")
                .and_then(|(_, rest)| rest.strip_suffix(')'))
                .map(str::to_string);
            Some(CargoBinstallPackage {
                name: name.to_string(),
                version: Some(version.to_string()),
                origin,
                state: InstallState::Installed,
            })
        })
        .collect()
}

/// A package as cargo's install registry describes it.
#[derive(Debug, Default)]
pub struct CargoBinstallPackage {
    pub name: String,
    pub version: Option<String>,
    /// Git URL or local path for non-registry installs; `None` means
    /// crates.io.
    pub origin: Option<String>,
    pub state: InstallState,
}

impl Package for CargoBinstallPackage {
    fn manager(&self) -> &str {
        ID
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_install_list() {
        let stdout = "\
cargo-binstall v1.10.7:
    cargo-binstall
ripgrep v14.1.1:
    rg
sccache v0.8.1 (/home/nick/src/sccache):
    sccache
";
        let packages = parse_install_list(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "cargo-binstall");
        assert_eq!(packages[0].version.as_deref(), Some("1.10.7"));
        assert_eq!(packages[0].origin, None);
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(
            packages[2].origin.as_deref(),
            Some("/home/nick/src/sccache")
        );
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("ripgrep@14.1.1")),
            "ripgrep@14.1.1"
        );
        assert_eq!(spec(&PackageRequest::parse("ripgrep")), "ripgrep");
    }
}
