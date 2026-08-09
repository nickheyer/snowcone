//! Clojure tools.deps backend for snowcone.
//!
//! Stub: discovery, capabilities, and database membership are wired;
//! the operations themselves are not implemented yet.

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Detection, Error, HostInfo, InstallState, ManagerKind, OpContext,
    Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "clojure";
const PROGRAMS: &[&str] = &["clojure", "clj"];

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
        "Clojure tools.deps"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "jvm"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
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
}

/// The package type this backend will produce once implemented.
#[derive(Debug)]
pub struct ClojurePackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for ClojurePackage {
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
