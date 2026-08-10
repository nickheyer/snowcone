//! Zig packages backend for snowcone.
//!
//! Zig has no global package manager. `zig fetch` resolves a URL or local
//! path into the content-addressed global cache and prints the resulting
//! hash; saving a dependency into a project's build.zig.zon is a project
//! concern snowcone stays out of. There is no registry to resolve names
//! against, no verb to list the cache, and no verb to remove one entry
//! from it. Install therefore treats each request as a fetchable URL or
//! path, and the other core operations return clear errors describing
//! zig's model rather than faking behavior on top of the cache.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, ManagerKind,
    OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "zig";
const PROGRAMS: &[&str] = &["zig"];

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

    /// The global cache is content-addressed hashes with no CLI verbs to
    /// inspect or edit it - these operations have nothing real to drive.
    fn cache_is_opaque(&self, operation: &str) -> Error {
        Error::Other(format!(
            "{ID}: {operation} is not possible - zig's global package cache is \
             content-addressed and the CLI has no verb to list or remove entries"
        ))
    }
}

/// `zig fetch` takes URLs and paths, not registry names - there is no
/// registry to resolve a `name@version` pin against.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but zig fetches URLs/paths and has no \
             registry to resolve versions against"
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
        "Zig packages"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "zig"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: install has no dry-run mode")));
        }
        for package in packages {
            self.run(self.cmd().arg("fetch").arg(&package.name), ctx)
                .await?;
        }
        Ok(())
    }

    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        Err(self.cache_is_opaque("remove"))
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Err(self.cache_is_opaque("list-installed"))
    }

    async fn info(&self, _name: &str) -> Result<Box<dyn Package>> {
        Err(self.cache_is_opaque("info"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("mach@0.4.0")]).is_err());
        assert!(
            reject_pins(&[PackageRequest::parse(
                "https://github.com/hexops/mach/archive/main.tar.gz"
            )])
            .is_ok()
        );
    }
}
