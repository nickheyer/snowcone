//! bauh backend for snowcone.
//!
//! bauh is a graphical manager for flatpak, snap, AUR, AppImage and web
//! apps; its command line only launches or configures the GUI itself
//! (`--tray`, `--settings`, `--reset`) and documents no non-interactive
//! verbs for installing, removing, listing, searching or upgrading
//! packages. Nothing here is scriptable, and faking success would be worse
//! than saying so: every core operation returns an error pointing at the
//! GUI, and the stub's SEARCH and UPGRADE capabilities are dropped because
//! the CLI verbs simply do not exist.

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Detection, Error, HostInfo, InstallState, ManagerKind, OpContext,
    Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "bauh";
const PROGRAMS: &[&str] = &["bauh"];

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
    fn gui_only(&self, operation: &str) -> Error {
        Error::Other(format!(
            "{ID}: {operation} is not scriptable - bauh only exposes its package operations through the GUI (run `bauh`)"
        ))
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "bauh"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Other
    }

    fn database_id(&self) -> &'static str {
        "bauh"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
    }

    async fn install(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        Err(self.gui_only("install"))
    }

    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        Err(self.gui_only("remove"))
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Err(self.gui_only("list-installed"))
    }

    async fn info(&self, _name: &str) -> Result<Box<dyn Package>> {
        Err(self.gui_only("info"))
    }
}

/// The package type this backend would produce - never constructed, because
/// bauh's CLI cannot report packages.
#[derive(Debug)]
pub struct BauhPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for BauhPackage {
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
    fn advertises_only_the_core_set() {
        assert_eq!(Manager.capabilities(), Capabilities::CORE);
        assert!(!Manager.capabilities().contains(Capabilities::SEARCH));
        assert!(!Manager.capabilities().contains(Capabilities::UPGRADE));
    }

    #[test]
    fn operations_explain_the_gui_limitation() {
        let message = Manager.gui_only("install").to_string();
        assert!(message.contains("bauh"));
        assert!(message.contains("GUI"));
        assert!(message.contains("install"));
    }
}
