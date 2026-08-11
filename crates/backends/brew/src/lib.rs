//! Homebrew (Linuxbrew) backend for snowcone.
//!
//! Formula-only: casks are macOS-side and never appear on Linux. brew runs
//! strictly unprivileged, so nothing elevates. Version pinning is not
//! advertised - brew's `name@version` formulae (`python@3.11`) are distinct
//! formula *names*, not a version selector, so a pinned request is rejected
//! rather than mistranslated.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "brew";
const PROGRAMS: &[&str] = &["brew"];

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
}

/// brew cannot install an arbitrary older version of a formula.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version; brew only installs a formula's current \
             version (versioned formulae like `python@3.11` are separate names)"
        ))),
        None => Ok(()),
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
        "Homebrew"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Universal
    }

    fn database_id(&self) -> &'static str {
        "brew"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::REFRESH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        let mut cmd = self.cmd().arg("install");
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        cmd = cmd.args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: uninstall has no dry-run mode")));
        }
        let cmd = self
            .cmd()
            .arg("uninstall")
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .cmd()
            .args(["list", "--versions"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_versions(&output.stdout))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .cmd()
            .args(["info", "--json=v2"])
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        parse_info(&parse_json("info output", &output.stdout)?)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .cmd()
            .arg("search")
            .arg(query)
            .capture(&self.elevator, None)
            .await?;
        // brew exits non-zero with `Error: No formulae or casks found for
        // "query".` (Library/Homebrew/cmd/search.rb) when nothing matches;
        // that is an empty result, not a failure.
        if !output.success() && output.stderr.contains("No formulae or casks found") {
            return Ok(Vec::new());
        }
        let output = output.require_success()?;
        Ok(parse_search(&output.stdout))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: update has no dry-run mode")));
        }
        self.run(self.cmd().arg("update"), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        let mut cmd = self.cmd().arg("upgrade");
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        cmd = cmd.args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .cmd()
            .args(["outdated", "--json=v2"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_outdated(&parse_json(
            "outdated output",
            &output.stdout,
        )?))
    }
}

/// `brew list --versions`: `name version [olderversion …]` per line; the
/// last token is the newest installed version.
fn parse_versions(stdout: &str) -> Vec<Box<dyn Package>> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            Some(Box::new(BrewPackage {
                name: name.to_string(),
                version: parts.last().map(str::to_string),
                state: InstallState::Installed,
                ..Default::default()
            }) as Box<dyn Package>)
        })
        .collect()
}

/// `brew info --json=v2`: the first entry of `formulae`.
fn parse_info(json: &Value) -> Option<BrewPackage> {
    let formula = json["formulae"].as_array()?.first()?;
    let stable = formula["versions"]["stable"].as_str().map(str::to_string);
    let installed = formula["installed"]
        .as_array()
        .and_then(|installs| installs.last())
        .and_then(|install| install["version"].as_str())
        .map(str::to_string);
    let mut package = BrewPackage {
        name: formula["name"].as_str()?.to_string(),
        description: formula["desc"].as_str().map(str::to_string),
        homepage: formula["homepage"].as_str().map(str::to_string),
        license: formula["license"].as_str().map(str::to_string),
        dependencies: formula["dependencies"].as_array().map(|deps| {
            deps.iter()
                .filter_map(|dep| dep.as_str())
                .map(str::to_string)
                .collect()
        }),
        ..Default::default()
    };
    match installed {
        Some(version) => {
            package.state = InstallState::Installed;
            if stable.is_some() && stable != Some(version.clone()) {
                package.latest_version = stable;
            }
            package.version = Some(version);
        }
        None => {
            package.state = InstallState::Available;
            package.version = stable;
        }
    }
    Some(package)
}

/// `brew search`: names one per line when piped, under `==> Formulae` /
/// `==> Casks` section headers. This backend is formula-only, so only the
/// formulae section is returned; names before any header (older brews) are
/// kept, since casks never print without their header.
fn parse_search(stdout: &str) -> Vec<Box<dyn Package>> {
    let mut in_formulae = true;
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            if let Some(section) = line.strip_prefix("==>") {
                in_formulae = section.trim() == "Formulae";
                return None;
            }
            in_formulae.then(|| {
                Box::new(BrewPackage {
                    name: line.to_string(),
                    state: InstallState::Available,
                    ..Default::default()
                }) as Box<dyn Package>
            })
        })
        .collect()
}

/// `brew outdated --json=v2`: `formulae` entries with installed and current
/// versions.
fn parse_outdated(json: &Value) -> Vec<Box<dyn Package>> {
    let Some(formulae) = json["formulae"].as_array() else {
        return Vec::new();
    };
    formulae
        .iter()
        .filter_map(|formula| {
            Some(Box::new(BrewPackage {
                name: formula["name"].as_str()?.to_string(),
                version: formula["installed_versions"]
                    .as_array()
                    .and_then(|versions| versions.last())
                    .and_then(|version| version.as_str())
                    .map(str::to_string),
                latest_version: formula["current_version"].as_str().map(str::to_string),
                state: InstallState::Upgradable,
                ..Default::default()
            }) as Box<dyn Package>)
        })
        .collect()
}

/// A package as brew describes it.
#[derive(Debug, Default)]
pub struct BrewPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for BrewPackage {
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
    fn parses_list_versions() {
        let packages = parse_versions("ripgrep 14.1.0\nfzf 0.46.0 0.46.1\n");
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name(), "fzf");
        assert_eq!(packages[1].version(), Some("0.46.1"));
        assert_eq!(packages[1].state(), InstallState::Installed);
    }

    #[test]
    fn parses_info_installed_formula() {
        let json: Value = serde_json::from_str(
            r#"{"formulae": [{
                "name": "ripgrep",
                "desc": "Search tool like grep and The Silver Searcher",
                "homepage": "https://github.com/BurntSushi/ripgrep",
                "license": "Unlicense",
                "versions": {"stable": "14.1.0"},
                "dependencies": ["pcre2"],
                "installed": [{"version": "14.0.3"}]
            }], "casks": []}"#,
        )
        .unwrap();
        let package = parse_info(&json).unwrap();
        assert_eq!(package.name, "ripgrep");
        assert_eq!(package.version.as_deref(), Some("14.0.3"));
        assert_eq!(package.latest_version.as_deref(), Some("14.1.0"));
        assert_eq!(package.state, InstallState::Installed);
        assert_eq!(package.dependencies, Some(vec!["pcre2".to_string()]));
    }

    #[test]
    fn parses_info_available_formula() {
        let json: Value = serde_json::from_str(
            r#"{"formulae": [{
                "name": "fzf",
                "versions": {"stable": "0.46.1"},
                "installed": []
            }], "casks": []}"#,
        )
        .unwrap();
        let package = parse_info(&json).unwrap();
        assert_eq!(package.version.as_deref(), Some("0.46.1"));
        assert_eq!(package.state, InstallState::Available);
    }

    #[test]
    fn parses_search_lines() {
        let packages = parse_search("==> Formulae\nripgrep\nripgrep-all\n");
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name(), "ripgrep");
    }

    #[test]
    fn search_skips_the_casks_section() {
        let stdout = "\
==> Formulae
ripgrep
ripgrep-all

==> Casks
repetier-server
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name(), "ripgrep");
        assert_eq!(packages[1].name(), "ripgrep-all");
    }

    #[test]
    fn search_without_headers_keeps_all_names() {
        let packages = parse_search("ripgrep\nripgrep-all\n");
        assert_eq!(packages.len(), 2);
    }

    #[test]
    fn parses_outdated_json() {
        let json: Value = serde_json::from_str(
            r#"{"formulae": [{
                "name": "fzf",
                "installed_versions": ["0.46.0"],
                "current_version": "0.46.1"
            }], "casks": []}"#,
        )
        .unwrap();
        let packages = parse_outdated(&json);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].version(), Some("0.46.0"));
        assert_eq!(packages[0].latest_version(), Some("0.46.1"));
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("python@3.11")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("ripgrep")]).is_ok());
    }
}
