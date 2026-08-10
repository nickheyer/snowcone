//! makedeb / MPR backend for snowcone.
//!
//! makedeb is a build tool - makepkg for .deb: it turns a PKGBUILD into an
//! archive and installs it through apt. Install therefore takes a *path*
//! as the request name: a PKGBUILD file, or a directory containing one,
//! passed via `--file` (the build itself runs in snowcone's working
//! directory, per makedeb's startdir semantics). makedeb refuses to run as
//! root and calls sudo itself for the install step, so snowcone never
//! elevates it. makedeb has no query verbs at all, so the stub's
//! list-installed and info capabilities are dropped; remove has no makedeb
//! verb either and delegates to elevated `dpkg -r` on the shared dpkg
//! database, which is the tool's documented removal path.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "makedeb";
const PROGRAMS: &[&str] = &["makedeb"];

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
        // Removal goes through dpkg, which is a given on any host that can
        // run makedeb.
        let dpkg_program =
            find_program("dpkg").ok_or_else(|| Error::Unavailable(ID.to_string()))?;
        Ok(Box::new(Manager {
            program,
            dpkg_program,
            elevator: Elevator::detect(host),
        }))
    }
}

struct Manager {
    program: PathBuf,
    dpkg_program: PathBuf,
    elevator: Elevator,
}

impl Manager {
    /// makedeb invocation. Never elevated - makedeb refuses root and sudos
    /// its own install step.
    fn cmd(&self) -> Cmd {
        Cmd::new(&self.program)
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

/// The version comes from the PKGBUILD being built; a `name@version`
/// request has nothing to resolve against.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but makedeb builds whatever the PKGBUILD carries"
        ))),
        None => Ok(()),
    }
}

/// Resolve an install request to the build file makedeb should be pointed
/// at: a directory means the PKGBUILD inside it.
fn build_file(name: &str) -> PathBuf {
    let path = Path::new(name);
    if path.is_dir() {
        path.join("PKGBUILD")
    } else {
        path.to_path_buf()
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "makedeb / MPR"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "dpkg"
    }

    /// The stub declared `CORE`, but makedeb factually has no verb to list
    /// or describe packages - those two capabilities are dropped rather
    /// than faked over the whole dpkg database.
    fn capabilities(&self) -> Capabilities {
        Capabilities::INSTALL | Capabilities::REMOVE
    }

    /// Install prompts through makedeb's own sudo; remove is elevated by
    /// snowcone (dpkg).
    fn needs_elevation(&self, operation: Operation) -> bool {
        matches!(operation, Operation::Install | Operation::Remove)
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: install has no dry-run mode")));
        }
        // One PKGBUILD per invocation - makedeb takes a single build file.
        for package in packages {
            let mut cmd = self
                .cmd()
                .args(["--sync-deps", "--install", "--file"])
                .arg(build_file(&package.name));
            if ctx.assume_yes {
                cmd = cmd.arg("--no-confirm");
            }
            self.run(cmd, ctx).await?;
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let mut cmd = Cmd::new(&self.dpkg_program).elevated(true);
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        cmd = cmd
            .arg("-r")
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Err(self.unsupported(Operation::ListInstalled))
    }

    async fn info(&self, _name: &str) -> Result<Box<dyn Package>> {
        Err(self.unsupported(Operation::Info))
    }
}

/// A package as makedeb describes it. makedeb has no query verbs, so
/// nothing constructs this today; it stays as the crate's package type for
/// the day the MPR grows one.
#[derive(Debug)]
pub struct MakedebPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for MakedebPackage {
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
    fn directory_requests_resolve_to_their_pkgbuild() {
        // `/` always exists and is a directory.
        assert_eq!(build_file("/"), Path::new("/PKGBUILD"));
    }

    #[test]
    fn file_requests_pass_through() {
        assert_eq!(
            build_file("/definitely/not/a/dir/PKGBUILD"),
            Path::new("/definitely/not/a/dir/PKGBUILD")
        );
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("./neofetch@7.1.0")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("./neofetch")]).is_ok());
    }
}
