//! Tiny Core extensions backend for snowcone.
//!
//! Tiny Core mounts .tcz extensions into RAM. `tce-load -wi` downloads and
//! loads them as the regular user - no elevation is needed and there are
//! no prompts for `assume_yes` to answer. The loaded-extension list comes
//! from the separate `tce-status -i` tool. Tiny Core has no uninstall
//! verb at all - extensions disappear by deleting the .tcz and its
//! onboot.lst entry and rebooting - so REMOVE is not advertised.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "tce-load";
const PROGRAMS: &[&str] = &["tce-load"];
const STATUS_PROGRAM: &str = "tce-status";

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
            // Listing loaded extensions is `tce-status`'s job, a separate
            // binary; resolved here so reads can fail with a clear message
            // when it is missing.
            status: find_program(STATUS_PROGRAM),
            elevator: Elevator::detect(host),
        }))
    }
}

struct Manager {
    program: PathBuf,
    status: Option<PathBuf>,
    elevator: Elevator,
}

impl Manager {
    fn cmd(&self) -> Cmd {
        Cmd::new(&self.program)
    }

    /// Read invocation of `tce-status` with a stable locale, so parsing
    /// survives i18n.
    fn status_query(&self) -> Result<Cmd> {
        let Some(status) = &self.status else {
            return Err(Error::Other(format!(
                "{ID}: `{STATUS_PROGRAM}` was not found on PATH"
            )));
        };
        Ok(Cmd::new(status).env("LC_ALL", "C"))
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

    /// Extensions currently loaded, from `tce-status -i`.
    async fn loaded(&self) -> Result<Vec<TceLoadPackage>> {
        let output = self
            .status_query()?
            .arg("-i")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_status(&output.stdout))
    }
}

/// Extension names may be given with or without the `.tcz` suffix; loaded
/// lists are compared without it.
fn extension_name(name: &str) -> &str {
    name.strip_suffix(".tcz").unwrap_or(name)
}

/// Tiny Core repositories carry exactly one build per extension: nothing to
/// pin.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but Tiny Core extensions are unversioned"
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
        "Tiny Core extensions"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "tce"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::INSTALL | Capabilities::LIST_INSTALLED | Capabilities::INFO
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: install has no dry-run mode")));
        }
        for package in packages {
            self.run(self.cmd().arg("-wi").arg(&package.name), ctx)
                .await?;
        }
        Ok(())
    }

    /// Tiny Core has no uninstall verb - removal is deleting the .tcz
    /// from the tce directory and its onboot.lst entry, then rebooting.
    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        Err(self.unsupported(Operation::Remove))
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .loaded()
            .await?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // The per-extension .tcz.info files live on the mirrors, not the
        // box; the loaded list is the only metadata guaranteed local.
        let target = extension_name(name);
        self.loaded()
            .await?
            .into_iter()
            .find(|package| package.name == target)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }
}

/// `tce-status -i`: one loaded extension per line, with or without the
/// `.tcz` suffix depending on the Tiny Core release.
fn parse_status(stdout: &str) -> Vec<TceLoadPackage> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| TceLoadPackage {
            name: extension_name(line).to_string(),
            state: InstallState::Installed,
        })
        .collect()
}

/// A package as Tiny Core describes it: extensions carry no version or
/// description locally.
#[derive(Debug)]
pub struct TceLoadPackage {
    pub name: String,
    pub state: InstallState,
}

impl Package for TceLoadPackage {
    fn manager(&self) -> &str {
        ID
    }

    fn name(&self) -> &str {
        &self.name
    }

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
    fn parses_status_with_suffix() {
        let packages = parse_status("Xlibs.tcz\nXprogs.tcz\nbash.tcz\n");
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[2].name, "bash");
        assert_eq!(packages[2].state, InstallState::Installed);
    }

    #[test]
    fn parses_status_without_suffix_and_skips_blanks() {
        let packages = parse_status("Xlibs\n\nbash\n");
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "Xlibs");
        assert_eq!(packages[1].name, "bash");
    }

    #[test]
    fn normalizes_extension_names() {
        assert_eq!(extension_name("bash.tcz"), "bash");
        assert_eq!(extension_name("bash"), "bash");
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("bash@5.2")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("bash")]).is_ok());
    }
}
