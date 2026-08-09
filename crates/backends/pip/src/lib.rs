//! pip backend for snowcone.
//!
//! Talks to whichever `pip`/`pip3` is on PATH and manages its
//! site-packages. `pip search` died with PyPI's XML-RPC API, so SEARCH is
//! not advertised. pip has no "upgrade everything" either - upgrade with no
//! arguments is composed from the outdated listing. On PEP 668
//! externally-managed distros pip refuses to touch site-packages; that
//! refusal is surfaced, never overridden.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "pip";
const PROGRAMS: &[&str] = &["pip", "pip3"];

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

    /// `pip list --format=json`, optionally restricted to outdated
    /// packages.
    async fn list_json(&self, outdated: bool) -> Result<Vec<PipPackage>> {
        let mut cmd = self
            .cmd()
            .args(["list", "--format=json", "--disable-pip-version-check"]);
        if outdated {
            cmd = cmd.arg("--outdated");
        }
        let output = cmd.capture(&self.elevator, None).await?.require_success()?;
        let json: Value = serde_json::from_str(&output.stdout).map_err(|error| Error::Parse {
            what: format!("{ID} list output"),
            detail: error.to_string(),
        })?;
        Ok(parse_list(&json, outdated))
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
        "pip"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "python"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let mut cmd = self.cmd().arg("install");
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        self.run(cmd.args(packages.iter().map(spec)), ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: uninstall has no dry-run mode")));
        }
        let mut cmd = self.cmd().arg("uninstall");
        if ctx.assume_yes {
            cmd = cmd.arg("-y");
        }
        cmd = cmd.args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .list_json(false)
            .await?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .cmd()
            .args(["show", "--disable-pip-version-check"])
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        parse_show(&output.stdout)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let specs: Vec<String> = if packages.is_empty() {
            // pip has no upgrade-all; the outdated listing is the target set.
            self.list_json(true)
                .await?
                .into_iter()
                .map(|package| package.name)
                .collect()
        } else {
            packages.iter().map(spec).collect()
        };
        if specs.is_empty() {
            return Ok(());
        }
        let mut cmd = self.cmd().args(["install", "--upgrade"]);
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        self.run(cmd.args(specs), ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .list_json(true)
            .await?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }
}

/// `pip list --format=json`: an array of `{name, version}` objects, plus
/// `latest_version` under `--outdated`.
fn parse_list(json: &Value, outdated: bool) -> Vec<PipPackage> {
    let Some(entries) = json.as_array() else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            Some(PipPackage {
                name: entry["name"].as_str()?.to_string(),
                version: entry["version"].as_str().map(str::to_string),
                latest_version: entry["latest_version"].as_str().map(str::to_string),
                state: if outdated {
                    InstallState::Upgradable
                } else {
                    InstallState::Installed
                },
                ..Default::default()
            })
        })
        .collect()
}

/// `pip show`: RFC-822ish `Key: Value` lines about an installed package.
fn parse_show(stdout: &str) -> Option<PipPackage> {
    let mut package = PipPackage {
        state: InstallState::Installed,
        ..Default::default()
    };
    for line in stdout.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if value.is_empty() {
            continue;
        }
        match key {
            "Name" => package.name = value.to_string(),
            "Version" => package.version = Some(value.to_string()),
            "Summary" => package.description = Some(value.to_string()),
            "Home-page" => package.homepage = Some(value.to_string()),
            "License" => package.license = Some(value.to_string()),
            "Requires" => {
                package.dependencies = Some(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|dep| !dep.is_empty())
                        .map(str::to_string)
                        .collect(),
                );
            }
            _ => {}
        }
    }
    (!package.name.is_empty()).then_some(package)
}

/// A package as pip describes it.
#[derive(Debug, Default)]
pub struct PipPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for PipPackage {
    fn manager(&self) -> &str {
        ID
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    fn latest_version(&self) -> Option<&str> {
        self.latest_version.as_deref()
    }

    fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    fn homepage(&self) -> Option<&str> {
        self.homepage.as_deref()
    }

    fn license(&self) -> Option<&str> {
        self.license.as_deref()
    }

    fn dependencies(&self) -> Option<Vec<String>> {
        self.dependencies.clone()
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_list_json() {
        let json: Value =
            serde_json::from_str(r#"[{"name": "requests", "version": "2.32.0"}]"#).unwrap();
        let packages = parse_list(&json, false);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "requests");
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn parses_outdated_list_json() {
        let json: Value = serde_json::from_str(
            r#"[{"name": "requests", "version": "2.31.0", "latest_version": "2.32.0"}]"#,
        )
        .unwrap();
        let packages = parse_list(&json, true);
        assert_eq!(packages[0].version.as_deref(), Some("2.31.0"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("2.32.0"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn parses_show_output() {
        let stdout = "\
Name: requests
Version: 2.32.0
Summary: Python HTTP for Humans.
Home-page: https://requests.readthedocs.io
License: Apache-2.0
Requires: certifi, charset-normalizer, idna, urllib3
Required-by:
";
        let package = parse_show(stdout).unwrap();
        assert_eq!(package.name, "requests");
        assert_eq!(
            package.description.as_deref(),
            Some("Python HTTP for Humans.")
        );
        assert_eq!(
            package.dependencies.as_ref().map(|deps| deps.len()),
            Some(4)
        );
        assert_eq!(package.state, InstallState::Installed);
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("requests@2.32.0")),
            "requests==2.32.0"
        );
        assert_eq!(spec(&PackageRequest::parse("requests")), "requests");
    }
}
