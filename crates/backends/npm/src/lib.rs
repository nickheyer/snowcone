//! npm backend for snowcone.
//!
//! Manages npm's global installs (`-g`): the per-project `node_modules`
//! world belongs to project tooling, not a system package CLI. Every query
//! uses npm's `--json` output. npm never prompts, so `assume_yes` has
//! nothing to do; `--dry-run` is native to install/uninstall/update.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "npm";
const PROGRAMS: &[&str] = &["npm"];

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

    /// Globally installed packages, from `npm ls -g`. npm exits non-zero
    /// for peer-dependency complaints while still printing valid JSON, so
    /// the JSON is authoritative and the exit code is not.
    async fn global_list(&self) -> Result<Vec<NpmPackage>> {
        let output = self
            .cmd()
            .args(["ls", "-g", "--depth=0", "--json"])
            .capture(&self.elevator, None)
            .await?;
        match serde_json::from_str::<Value>(&output.stdout) {
            Ok(json) => Ok(parse_ls(&json)),
            Err(_) => {
                output.require_success()?;
                Ok(Vec::new())
            }
        }
    }
}

/// `name@version` when the request pins one, bare name otherwise.
fn spec(request: &PackageRequest) -> String {
    match &request.version {
        Some(version) => format!("{}@{version}", request.name),
        None => request.name.clone(),
    }
}

fn parse_json(what: &str, stdout: &str) -> Result<Value> {
    serde_json::from_str(stdout).map_err(|error| Error::Parse {
        what: format!("{ID} {what}"),
        detail: error.to_string(),
    })
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "npm"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "node"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let mut cmd = self.cmd().args(["install", "-g"]);
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        self.run(cmd.args(packages.iter().map(spec)), ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let mut cmd = self.cmd().args(["uninstall", "-g"]);
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        cmd = cmd.args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .global_list()
            .await?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .cmd()
            .args(["view", "--json"])
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        let json = parse_json("view output", &output.stdout)?;
        let mut package = parse_view(&json).ok_or_else(|| Error::NotFound(name.to_string()))?;
        // The registry view says nothing about the local install.
        if let Some(installed) = self
            .global_list()
            .await?
            .into_iter()
            .find(|installed| installed.name == package.name)
        {
            package.state = InstallState::Installed;
            if installed.version.is_some() && installed.version != package.version {
                package.latest_version = package.version.take();
                package.version = installed.version;
            }
        }
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .cmd()
            .args(["search", "--json"])
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_search(&parse_json("search output", &output.stdout)?)
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let mut cmd = if packages.is_empty() {
            self.cmd().args(["update", "-g"])
        } else {
            // `install name@latest` upgrades past whatever semver range the
            // original install recorded; `update` would respect it.
            self.cmd().args(["install", "-g"]).args(
                packages
                    .iter()
                    .map(|package| format!("{}@latest", package.name)),
            )
        };
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        // `npm outdated` exits 1 whenever anything is outdated; only an
        // unparseable stdout marks a real failure.
        let output = self
            .cmd()
            .args(["outdated", "-g", "--json"])
            .capture(&self.elevator, None)
            .await?;
        if output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(
            parse_outdated(&parse_json("outdated output", &output.stdout)?)
                .into_iter()
                .map(|package| Box::new(package) as Box<dyn Package>)
                .collect(),
        )
    }
}

/// `npm ls --json`: `dependencies` maps name → `{version, …}`.
fn parse_ls(json: &Value) -> Vec<NpmPackage> {
    let Some(dependencies) = json["dependencies"].as_object() else {
        return Vec::new();
    };
    dependencies
        .iter()
        .map(|(name, entry)| NpmPackage {
            name: name.clone(),
            version: entry["version"].as_str().map(str::to_string),
            state: InstallState::Installed,
            ..Default::default()
        })
        .collect()
}

/// `npm view --json`: one registry document — except that some npm
/// versions wrap it in an array, in which case the last (newest) entry
/// wins. `license` is a string on modern packages and an object on ancient
/// ones.
fn parse_view(json: &Value) -> Option<NpmPackage> {
    let json = match json.as_array() {
        Some(items) => items.last()?,
        None => json,
    };
    let name = json["name"].as_str()?;
    let license = json["license"]
        .as_str()
        .map(str::to_string)
        .or_else(|| json["license"]["type"].as_str().map(str::to_string));
    Some(NpmPackage {
        name: name.to_string(),
        version: json["version"].as_str().map(str::to_string),
        description: json["description"].as_str().map(str::to_string),
        homepage: json["homepage"].as_str().map(str::to_string),
        license,
        dependencies: json["dependencies"]
            .as_object()
            .map(|map| map.keys().cloned().collect()),
        state: InstallState::Available,
        ..Default::default()
    })
}

/// `npm search --json`: an array of registry entries.
fn parse_search(json: &Value) -> Vec<NpmPackage> {
    let Some(entries) = json.as_array() else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            Some(NpmPackage {
                name: entry["name"].as_str()?.to_string(),
                version: entry["version"].as_str().map(str::to_string),
                description: entry["description"].as_str().map(str::to_string),
                state: InstallState::Available,
                ..Default::default()
            })
        })
        .collect()
}

/// `npm outdated --json`: maps name → `{current, wanted, latest, …}`.
fn parse_outdated(json: &Value) -> Vec<NpmPackage> {
    let Some(entries) = json.as_object() else {
        return Vec::new();
    };
    entries
        .iter()
        .map(|(name, entry)| NpmPackage {
            name: name.clone(),
            version: entry["current"].as_str().map(str::to_string),
            latest_version: entry["latest"].as_str().map(str::to_string),
            state: InstallState::Upgradable,
            ..Default::default()
        })
        .collect()
}

/// A package as npm describes it.
#[derive(Debug, Default)]
pub struct NpmPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for NpmPackage {
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
    fn parses_global_ls() {
        let json: Value = serde_json::from_str(
            r#"{"name": "lib", "dependencies": {
                "corepack": {"version": "0.28.0"},
                "npm": {"version": "10.8.0"}
            }}"#,
        )
        .unwrap();
        let packages = parse_ls(&json);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "corepack");
        assert_eq!(packages[0].version.as_deref(), Some("0.28.0"));
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn parses_view_with_object_license() {
        let json: Value = serde_json::from_str(
            r#"{"name": "left-pad", "version": "1.3.0",
                "description": "String left pad",
                "license": {"type": "WTFPL"},
                "dependencies": {"pad-core": "^1.0.0"}}"#,
        )
        .unwrap();
        let package = parse_view(&json).unwrap();
        assert_eq!(package.license.as_deref(), Some("WTFPL"));
        assert_eq!(package.dependencies, Some(vec!["pad-core".to_string()]));
    }

    #[test]
    fn parses_array_wrapped_view() {
        let json: Value = serde_json::from_str(
            r#"[{"name": "left-pad", "version": "1.2.0", "license": "WTFPL"},
                {"name": "left-pad", "version": "1.3.0", "license": "WTFPL"}]"#,
        )
        .unwrap();
        let package = parse_view(&json).unwrap();
        assert_eq!(package.version.as_deref(), Some("1.3.0"));
    }

    #[test]
    fn parses_search_array() {
        let json: Value = serde_json::from_str(
            r#"[{"name": "express", "version": "4.19.0", "description": "Fast web framework"}]"#,
        )
        .unwrap();
        let packages = parse_search(&json);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "express");
    }

    #[test]
    fn parses_outdated_map() {
        let json: Value = serde_json::from_str(
            r#"{"corepack": {"current": "0.28.0", "wanted": "0.29.0", "latest": "0.29.0"}}"#,
        )
        .unwrap();
        let packages = parse_outdated(&json);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].version.as_deref(), Some("0.28.0"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("0.29.0"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("@types/node@22.0.0")),
            "@types/node@22.0.0"
        );
        assert_eq!(spec(&PackageRequest::parse("express")), "express");
    }
}
