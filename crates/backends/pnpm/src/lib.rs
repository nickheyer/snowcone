//! pnpm backend for snowcone.
//!
//! Manages pnpm's global installs (`-g`), the same user-level stance as the
//! npm backend: the per-project world belongs to project tooling. `pnpm
//! list -g --json` answers with the npm shape wrapped in an array of
//! projects. `pnpm outdated` exits 1 whenever something is outdated, and
//! its `--no-table` list view is parsed instead of the default box-drawing
//! table. pnpm never prompts for these verbs and has no dry-run flags, so
//! `assume_yes` has nothing to do and `dry_run` errors. pnpm has no
//! registry-view verb this backend trusts, so `info` only describes
//! installed globals.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "pnpm";
const PROGRAMS: &[&str] = &["pnpm"];

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

    /// Read invocation with a stable locale, so parsing survives i18n.
    fn query(&self) -> Cmd {
        Cmd::new(&self.program).env("LC_ALL", "C")
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

    fn no_dry_run(&self, operation: &str) -> Error {
        Error::Other(format!("{ID}: {operation} has no dry-run mode"))
    }

    /// Globally installed packages, from `pnpm list -g --json`.
    async fn global_list(&self) -> Result<Vec<PnpmPackage>> {
        let output = self
            .cmd()
            .args(["list", "-g", "--depth=0", "--json"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_list(&parse_json("list output", &output.stdout)?))
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
        "pnpm"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "node"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        let cmd = self
            .cmd()
            .args(["add", "-g"])
            .args(packages.iter().map(spec));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        let cmd = self
            .cmd()
            .args(["remove", "-g"])
            .args(packages.iter().map(|package| package.name.as_str()));
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
        self.global_list()
            .await?
            .into_iter()
            .find(|package| package.name == name)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        let cmd = if packages.is_empty() {
            self.cmd().args(["update", "-g"])
        } else {
            // `add name@latest` upgrades past whatever semver range the
            // original install recorded; `update` would respect it.
            self.cmd()
                .args(["add", "-g"])
                .args(packages.iter().map(|package| match &package.version {
                    Some(version) => format!("{}@{version}", package.name),
                    None => format!("{}@latest", package.name),
                }))
        };
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        // `pnpm outdated` exits 1 whenever anything is outdated; success
        // with empty output means everything is current.
        let output = self
            .query()
            .args(["outdated", "-g", "--no-table"])
            .capture(&self.elevator, None)
            .await?;
        if output.stdout.trim().is_empty() {
            output.require_success()?;
            return Ok(Vec::new());
        }
        Ok(parse_outdated(&output.stdout)
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }
}

/// `pnpm list -g --json`: an array of projects (one for the global dir),
/// each with an npm-style `dependencies` map of name → `{version, …}`; a
/// bare object is tolerated too.
fn parse_list(json: &Value) -> Vec<PnpmPackage> {
    let projects = json
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(std::slice::from_ref(json));
    projects
        .iter()
        .filter_map(|project| project["dependencies"].as_object())
        .flat_map(|dependencies| {
            dependencies.iter().map(|(name, entry)| PnpmPackage {
                name: name.clone(),
                version: entry["version"].as_str().map(str::to_string),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `pnpm outdated --no-table`: the package name on one line (possibly with
/// a parenthesized dependency-type suffix), `current => latest` on the
/// next, entries separated by blank lines.
fn parse_outdated(stdout: &str) -> Vec<PnpmPackage> {
    let mut packages = Vec::new();
    let mut pending: Option<String> = None;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            pending = None;
            continue;
        }
        if let Some((current, latest)) = line.split_once("=>") {
            if let Some(name) = pending.take()
                && let (Some(current), Some(latest)) = (
                    current.split_whitespace().next(),
                    latest.split_whitespace().next(),
                )
            {
                packages.push(PnpmPackage {
                    name,
                    version: Some(current.to_string()),
                    latest_version: Some(latest.to_string()),
                    state: InstallState::Upgradable,
                });
            }
            continue;
        }
        let name = line.split_once(" (").map_or(line, |(name, _)| name);
        pending = Some(name.to_string());
    }
    packages
}

/// A package as pnpm describes it.
#[derive(Debug, Default)]
pub struct PnpmPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub state: InstallState,
}

impl Package for PnpmPackage {
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

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_array_wrapped_global_list() {
        let json: Value = serde_json::from_str(
            r#"[{"path": "/home/nick/.local/share/pnpm/global/5", "private": false,
                "dependencies": {
                    "pnpm": {"from": "pnpm", "version": "9.4.0",
                             "resolved": "https://registry.npmjs.org/pnpm/-/pnpm-9.4.0.tgz"},
                    "typescript": {"from": "typescript", "version": "5.5.3",
                                   "resolved": "https://registry.npmjs.org/typescript/-/typescript-5.5.3.tgz"}
                }}]"#,
        )
        .unwrap();
        let packages = parse_list(&json);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "pnpm");
        assert_eq!(packages[0].version.as_deref(), Some("9.4.0"));
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn parses_unwrapped_global_list() {
        let json: Value = serde_json::from_str(
            r#"{"dependencies": {"typescript": {"version": "5.5.3"}}}"#,
        )
        .unwrap();
        let packages = parse_list(&json);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "typescript");
    }

    #[test]
    fn parses_outdated_list_view() {
        let stdout = "\
typescript
5.4.5 => 5.5.3

@angular/cli (dependencies)
17.0.0 => 18.0.5
";
        let packages = parse_outdated(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "typescript");
        assert_eq!(packages[0].version.as_deref(), Some("5.4.5"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("5.5.3"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
        assert_eq!(packages[1].name, "@angular/cli");
        assert_eq!(packages[1].version.as_deref(), Some("17.0.0"));
    }

    #[test]
    fn empty_outdated_output_parses_to_nothing() {
        assert!(parse_outdated("").is_empty());
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("@types/node@22.0.0")),
            "@types/node@22.0.0"
        );
        assert_eq!(spec(&PackageRequest::parse("typescript")), "typescript");
    }
}
