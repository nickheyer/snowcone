//! pipx backend for snowcone.
//!
//! pipx keeps each application in its own venv, and its CLI works on one
//! package at a time — so batch operations loop rather than pass a list.
//! Everything pipx knows about its installs comes from `pipx list --json`.
//! No search (pipx installs from PyPI but cannot query it), no refresh, no
//! outdated listing.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "pipx";
const PROGRAMS: &[&str] = &["pipx"];

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

    async fn venvs(&self) -> Result<Vec<PipxPackage>> {
        let output = self
            .cmd()
            .args(["list", "--json"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        let json: Value = serde_json::from_str(&output.stdout).map_err(|error| Error::Parse {
            what: format!("{ID} list output"),
            detail: error.to_string(),
        })?;
        Ok(parse_list(&json))
    }
}

/// `name==version` when the request pins one, bare name otherwise.
fn spec(request: &PackageRequest) -> String {
    match &request.version {
        Some(version) => format!("{}=={version}", request.name),
        None => request.name.clone(),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "pipx"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "python"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::UPGRADE | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: install has no dry-run mode")));
        }
        for package in packages {
            self.run(self.cmd().arg("install").arg(spec(package)), ctx)
                .await?;
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: uninstall has no dry-run mode")));
        }
        for package in packages {
            self.run(self.cmd().arg("uninstall").arg(&package.name), ctx)
                .await?;
        }
        Ok(())
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .venvs()
            .await?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        self.venvs()
            .await?
            .into_iter()
            .find(|package| package.name == name)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: upgrade has no dry-run mode")));
        }
        if packages.is_empty() {
            return self.run(self.cmd().arg("upgrade-all"), ctx).await;
        }
        for package in packages {
            self.run(self.cmd().arg("upgrade").arg(&package.name), ctx)
                .await?;
        }
        Ok(())
    }
}

/// `pipx list --json`: `venvs` maps venv name → metadata about its main
/// package.
fn parse_list(json: &Value) -> Vec<PipxPackage> {
    let Some(venvs) = json["venvs"].as_object() else {
        return Vec::new();
    };
    venvs
        .iter()
        .map(|(venv, entry)| {
            let main = &entry["metadata"]["main_package"];
            PipxPackage {
                name: main["package"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| venv.clone()),
                version: main["package_version"].as_str().map(str::to_string),
                state: InstallState::Installed,
            }
        })
        .collect()
}

/// A package as pipx describes it.
#[derive(Debug, Default)]
pub struct PipxPackage {
    pub name: String,
    pub version: Option<String>,
    pub state: InstallState,
}

impl Package for PipxPackage {
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
    fn parses_venv_list() {
        let json: Value = serde_json::from_str(
            r#"{"pipx_spec_version": "0.1", "venvs": {"black": {"metadata": {
                "main_package": {
                    "package": "black",
                    "package_version": "24.4.2",
                    "apps": ["black", "blackd"]
                }
            }}}}"#,
        )
        .unwrap();
        let packages = parse_list(&json);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "black");
        assert_eq!(packages[0].version.as_deref(), Some("24.4.2"));
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn empty_venvs_parse_to_nothing() {
        let json: Value =
            serde_json::from_str(r#"{"pipx_spec_version": "0.1", "venvs": {}}"#).unwrap();
        assert!(parse_list(&json).is_empty());
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("black@24.4.2")),
            "black==24.4.2"
        );
        assert_eq!(spec(&PackageRequest::parse("black")), "black");
    }
}
