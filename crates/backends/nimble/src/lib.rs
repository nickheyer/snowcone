//! Nimble backend for snowcone.
//!
//! Stub: discovery, capabilities, and database membership are wired;
//! the operations themselves are not implemented yet.

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Detection, Error, HostInfo, InstallState, ManagerKind, OpContext,
    Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "nimble";
const PROGRAMS: &[&str] = &["nimble"];

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

    fn create(&self, _host: &HostInfo) -> Result<Box<dyn PackageManager>> {
        Ok(Box::new(Manager))
    }
}

struct Manager;

impl Manager {
    fn todo(&self, what: &str) -> Error {
        Error::Other(format!("{ID}: {what} not implemented yet"))
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Nimble"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "nimble"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::REFRESH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        Err(self.todo("install"))
    }

    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        Err(self.todo("remove"))
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Err(self.todo("list-installed"))
    }

    async fn info(&self, _name: &str) -> Result<Box<dyn Package>> {
        Err(self.todo("info"))
    }

    async fn search(&self, _query: &str) -> Result<Vec<Box<dyn Package>>> {
        Err(self.todo("search"))
    }

    async fn refresh(&self, _ctx: &OpContext) -> Result<()> {
        Err(self.todo("refresh"))
    }

    async fn upgrade(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        Err(self.todo("upgrade"))
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        Err(self.todo("list-outdated"))
    }
}

/// The package type this backend will produce once implemented.
#[derive(Debug)]
pub struct NimblePackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for NimblePackage {
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
