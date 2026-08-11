//! PDM backend for snowcone.
//!
//! PDM is a project manager, not a system package manager: `add`,
//! `remove`, `list`, `update`, and `outdated` all operate on the
//! pyproject.toml project in the *current working directory*, while
//! `show` reaches the remote index. That cwd-scoped contract is
//! deliberate. `pdm search` rode PyPI's XML-RPC search API, which is
//! disabled, and is deprecated upstream, so SEARCH is not advertised. pdm
//! renders human output through rich, so the installed listing uses
//! `--json` and the other reads run with NO_COLOR under LC_ALL=C (the
//! outdated parser also strips rich's box drawing); `--dry-run` is native
//! to add, remove, and update.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "pdm";
const PROGRAMS: &[&str] = &["pdm"];

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
    /// Mutating invocation, in the user's locale (output is passed through).
    fn cmd(&self) -> Cmd {
        Cmd::new(&self.program)
    }

    /// Read invocation with a stable locale and rich's coloring off, so
    /// line output parses cleanly.
    fn query(&self) -> Cmd {
        Cmd::new(&self.program)
            .env("LC_ALL", "C")
            .env("NO_COLOR", "1")
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

    /// Shared flags for mutating commands: the native `--dry-run` (add,
    /// remove, and update all support it). pdm has no documented yes-flag;
    /// the rare prompt simply runs interactively.
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(subcommand);
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        cmd
    }

    /// Installed packages in the project environment, from
    /// `pdm list --json`.
    async fn list_json(&self) -> Result<Vec<PdmPackage>> {
        let output = self
            .query()
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

/// `name==version` when the request pins one, bare name otherwise
/// (`pdm add` takes PEP 508 requirement specifiers).
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
        "PDM"
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
        let cmd = self.mutation("add", ctx).args(packages.iter().map(spec));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = self
            .mutation("remove", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.list_json().await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query()
            .arg("show")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        let mut package =
            parse_show(&output.stdout).ok_or_else(|| Error::NotFound(name.to_string()))?;
        // `pdm show` describes the index side only; the project listing
        // fills in the installed state and version.
        if let Some(installed) = self
            .list_json()
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

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if packages.is_empty() {
            return self.run(self.mutation("update", ctx), ctx).await;
        }
        let (pinned, constrained): (Vec<&PackageRequest>, Vec<&PackageRequest>) = packages
            .iter()
            .partition(|package| package.version.is_some());
        if !constrained.is_empty() {
            let cmd = self
                .mutation("update", ctx)
                .args(constrained.iter().map(|package| package.name.as_str()));
            self.run(cmd, ctx).await?;
        }
        // `pdm update` honors the pyproject constraint; moving to a pinned
        // version means rewriting the constraint, which is `add`.
        if !pinned.is_empty() {
            let cmd = self
                .mutation("add", ctx)
                .args(pinned.iter().map(|package| spec(package)));
            self.run(cmd, ctx).await?;
        }
        Ok(())
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("outdated")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_outdated(&output.stdout)))
    }
}

fn boxed(packages: Vec<PdmPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `pdm list --json`: an array of `{name, version, …}` records describing
/// the project environment.
fn parse_list(json: &Value) -> Vec<PdmPackage> {
    let Some(entries) = json.as_array() else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            Some(PdmPackage {
                name: entry["name"].as_str()?.to_string(),
                version: entry["version"].as_str().map(str::to_string),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `pdm show`: `Key: Value` metadata lines about an index package
/// (`Name`, `Latest version`, `Summary`, `License`, `Homepage`, …).
fn parse_show(stdout: &str) -> Option<PdmPackage> {
    let mut package = PdmPackage {
        state: InstallState::Available,
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
            "Latest version" | "Version" => package.version = Some(value.to_string()),
            "Summary" => package.description = Some(value.to_string()),
            "License" => package.license = Some(value.to_string()),
            "Homepage" => package.homepage = Some(value.to_string()),
            _ => {}
        }
    }
    (!package.name.is_empty()).then_some(package)
}

/// `pdm outdated`: a rich box table whose columns are Package | Installed
/// | Pinned | Latest (pdm 2.19 inserted a Groups column after Package) -
/// cells may be separated by box-drawing `│` characters, ASCII pipes, or
/// plain whitespace. The version columns are always the last three, so
/// rows anchor from the end; a row reduced to three cells (an empty
/// Pinned cell drops out of the split) still parses. Header and border
/// rows fail the version check.
fn parse_outdated(stdout: &str) -> Vec<PdmPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let cells: Vec<&str> = if line.contains(['│', '|']) {
                line.split(['│', '|'])
                    .map(str::trim)
                    .filter(|cell| !cell.is_empty())
                    .collect()
            } else {
                line.split_whitespace().collect()
            };
            let (name, current, latest) = match cells[..] {
                [name, .., current, _pinned, latest] => (name, current, latest),
                [name, current, latest] => (name, current, latest),
                _ => return None,
            };
            if !current.starts_with(|c: char| c.is_ascii_digit())
                || !latest.starts_with(|c: char| c.is_ascii_digit())
            {
                return None;
            }
            Some(PdmPackage {
                name: name.to_string(),
                version: Some(current.to_string()),
                latest_version: Some(latest.to_string()),
                state: InstallState::Upgradable,
                ..Default::default()
            })
        })
        .collect()
}

/// A package as pdm describes it.
#[derive(Debug, Default)]
pub struct PdmPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub state: InstallState,
}

impl Package for PdmPackage {
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

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_list_json() {
        let json: Value = serde_json::from_str(
            r#"[{"name": "requests", "version": "2.32.3", "location": ""},
                {"name": "urllib3", "version": "2.2.2", "location": ""}]"#,
        )
        .unwrap();
        let packages = parse_list(&json);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "requests");
        assert_eq!(packages[0].version.as_deref(), Some("2.32.3"));
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn parses_show_output() {
        let stdout = "\
Name:                  requests
Latest version:        2.32.3
Summary:               Python HTTP for Humans.
Keywords:              http,requests
Author:                Kenneth Reitz
License:               Apache-2.0
Requires python:       >=3.8
Homepage:              https://requests.readthedocs.io
";
        let package = parse_show(stdout).unwrap();
        assert_eq!(package.name, "requests");
        assert_eq!(package.version.as_deref(), Some("2.32.3"));
        assert_eq!(
            package.description.as_deref(),
            Some("Python HTTP for Humans.")
        );
        assert_eq!(package.license.as_deref(), Some("Apache-2.0"));
        assert_eq!(
            package.homepage.as_deref(),
            Some("https://requests.readthedocs.io")
        );
        assert_eq!(package.state, InstallState::Available);
    }

    #[test]
    fn parses_outdated_box_table() {
        // pdm <= 2.15: Package | Installed | Pinned | Latest, rich ROUNDED
        // box. The urllib3 row's empty Pinned cell drops to three cells.
        let stdout = "\
╭──────────┬───────────┬────────┬────────╮
│ Package  │ Installed │ Pinned │ Latest │
├──────────┼───────────┼────────┼────────┤
│ requests │ 2.31.0    │ 2.31.0 │ 2.32.3 │
│ urllib3  │ 2.2.1     │        │ 2.2.2  │
╰──────────┴───────────┴────────┴────────╯
";
        let packages = parse_outdated(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "requests");
        assert_eq!(packages[0].version.as_deref(), Some("2.31.0"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("2.32.3"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
        assert_eq!(packages[1].name, "urllib3");
        assert_eq!(packages[1].latest_version.as_deref(), Some("2.2.2"));
    }

    #[test]
    fn parses_outdated_box_table_with_groups() {
        // pdm 2.19+ inserts a Groups column after Package.
        let stdout = "\
╭──────────┬─────────┬───────────┬────────┬────────╮
│ Package  │ Groups  │ Installed │ Pinned │ Latest │
├──────────┼─────────┼───────────┼────────┼────────┤
│ requests │ default │ 2.31.0    │ 2.31.0 │ 2.32.3 │
╰──────────┴─────────┴───────────┴────────┴────────╯
";
        let packages = parse_outdated(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "requests");
        assert_eq!(packages[0].version.as_deref(), Some("2.31.0"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("2.32.3"));
    }

    #[test]
    fn parses_outdated_plain_table() {
        let stdout = "\
Package  Installed Pinned Latest
requests 2.31.0    2.31.0 2.32.3
";
        let packages = parse_outdated(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "requests");
        assert_eq!(packages[0].version.as_deref(), Some("2.31.0"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("2.32.3"));
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("requests@2.32.3")),
            "requests==2.32.3"
        );
        assert_eq!(spec(&PackageRequest::parse("requests")), "requests");
    }
}
