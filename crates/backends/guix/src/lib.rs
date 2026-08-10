//! Guix backend for snowcone.
//!
//! Guix profiles are per-user and the daemon does the building, so no
//! operation is ever elevated - there is no root path here at all, and
//! guix never asks y/n questions, so `assume_yes` has nothing to do. The
//! transactional verbs take a native `--dry-run`. `refresh` maps to
//! `guix pull`, which updates guix itself plus its channels - that is what
//! "the index" means here. `guix search`/`show` emit recutils records
//! (blank-line-separated `key: value` fields, `+`-continued), and
//! `--list-installed`/`--list-available` are tab-separated. Guix has no
//! outdated verb, so list-outdated diffs those two listings. Pins use
//! guix's native `name@version` specs.

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "guix";
const PROGRAMS: &[&str] = &["guix"];

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

    /// The profile's contents, from the tab-separated `--list-installed`.
    async fn installed(&self) -> Result<Vec<(String, String)>> {
        let output = self
            .query()
            .args(["package", "--list-installed"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_columns(&output.stdout))
    }
}

/// `name@version` (guix's native version spec) when the request pins one,
/// bare name otherwise.
fn spec(request: &PackageRequest) -> String {
    match &request.version {
        Some(version) => format!("{}@{version}", request.name),
        None => request.name.clone(),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Guix"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Universal
    }

    fn database_id(&self) -> &'static str {
        "guix"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::REFRESH
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
        let mut cmd = self.cmd().arg("remove");
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        cmd = cmd.args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .installed()
            .await?
            .into_iter()
            .map(|(name, version)| {
                Box::new(GuixPackage {
                    name,
                    version: Some(version).filter(|version| !version.is_empty()),
                    state: InstallState::Installed,
                    ..Default::default()
                }) as Box<dyn Package>
            })
            .collect())
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
        // `show` prints one record per available version; keep the newest.
        let mut package = parse_records(&output.stdout)
            .into_iter()
            .filter(|package| package.name == name)
            .reduce(|best, candidate| {
                match (candidate.version.as_deref(), best.version.as_deref()) {
                    (Some(new), Some(old)) if version_newer(new, old) => candidate,
                    (Some(_), None) => candidate,
                    _ => best,
                }
            })
            .ok_or_else(|| Error::NotFound(name.to_string()))?;
        // The record describes the channel side only; the profile listing
        // says whether (and at which version) it is installed.
        if let Some((_, installed)) = self
            .installed()
            .await?
            .into_iter()
            .find(|(installed, _)| installed == &package.name)
        {
            let installed = Some(installed).filter(|version| !version.is_empty());
            package.state = InstallState::Installed;
            if installed.is_some() && installed != package.version {
                if let (Some(latest), Some(current)) = (package.version.as_deref(), installed.as_deref())
                    && version_newer(latest, current)
                {
                    package.state = InstallState::Upgradable;
                }
                package.latest_version = package.version.take();
                package.version = installed;
            }
        }
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("search")
            .arg(query)
            .capture(&self.elevator, None)
            .await?;
        // Some guix versions exit non-zero on an empty result set; an
        // empty stdout is authoritative either way.
        if !output.success() && output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        let output = output.require_success()?;
        Ok(parse_records(&output.stdout)
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!(
                "{ID}: refresh (guix pull) has no dry-run mode"
            )));
        }
        self.run(self.cmd().arg("pull"), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        // Targeted upgrades go through `install`: profile transactions
        // replace the installed entry, pins work, and `guix upgrade`'s
        // regexp matching (where `gtk+` is a pattern) is sidestepped.
        let mut cmd = if packages.is_empty() {
            self.cmd().arg("upgrade")
        } else {
            self.cmd().arg("install").args(packages.iter().map(spec))
        };
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let installed = self.installed().await?;
        let output = self
            .query()
            .args(["package", "--list-available"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        let available = parse_columns(&output.stdout);
        Ok(outdated(&installed, &available)
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }
}

/// Tab-separated `guix package --list-installed`/`--list-available` rows:
/// name and version first, then output/location columns.
fn parse_columns(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let name = parts.next()?.trim();
            let version = parts.next()?.trim();
            (!name.is_empty()).then(|| (name.to_string(), version.to_string()))
        })
        .collect()
}

/// recutils records as printed by `guix search`/`guix show`: blank-line
/// separated `key: value` fields, with wrapped values continued on lines
/// beginning with `+`. The short `synopsis` becomes the description.
fn parse_records(stdout: &str) -> Vec<GuixPackage> {
    let mut packages = Vec::new();
    let mut current: Option<GuixPackage> = None;
    let mut last_key = String::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            if let Some(package) = current.take()
                && !package.name.is_empty()
            {
                packages.push(package);
            }
            last_key.clear();
            continue;
        }
        if let Some(continued) = line.strip_prefix('+') {
            if last_key == "synopsis"
                && let Some(description) = current.as_mut().and_then(|p| p.description.as_mut())
            {
                let continued = continued.trim();
                if !continued.is_empty() {
                    description.push(' ');
                    description.push_str(continued);
                }
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        let package = current.get_or_insert_with(GuixPackage::default);
        last_key = key.to_string();
        if value.is_empty() {
            continue;
        }
        match key {
            "name" => package.name = value.to_string(),
            "version" => package.version = Some(value.to_string()),
            "synopsis" => package.description = Some(value.to_string()),
            "homepage" => package.homepage = Some(value.to_string()),
            "license" => package.license = Some(value.to_string()),
            "dependencies" => {
                package.dependencies = Some(
                    value
                        .split_whitespace()
                        .filter_map(|dep| dep.split('@').next())
                        .map(str::to_string)
                        .collect(),
                );
            }
            _ => {}
        }
    }
    if let Some(package) = current
        && !package.name.is_empty()
    {
        packages.push(package);
    }
    packages
}

/// Best-effort "is `a` newer than `b`": dot/dash/underscore-separated
/// segments compare numerically when both sides are numeric, lexically
/// otherwise; a longer version wins over its own prefix.
fn version_newer(a: &str, b: &str) -> bool {
    use std::cmp::Ordering;
    let mut left = a.split(['.', '-', '_']);
    let mut right = b.split(['.', '-', '_']);
    loop {
        match (left.next(), right.next()) {
            (None, None) => return false,
            (Some(_), None) => return true,
            (None, Some(_)) => return false,
            (Some(l), Some(r)) => {
                let ordering = match (l.parse::<u64>(), r.parse::<u64>()) {
                    (Ok(l), Ok(r)) => l.cmp(&r),
                    _ => l.cmp(r),
                };
                match ordering {
                    Ordering::Greater => return true,
                    Ordering::Less => return false,
                    Ordering::Equal => {}
                }
            }
        }
    }
}

/// Installed entries whose best available version is newer.
fn outdated(installed: &[(String, String)], available: &[(String, String)]) -> Vec<GuixPackage> {
    let mut best: HashMap<&str, &str> = HashMap::new();
    for (name, version) in available {
        let slot = best.entry(name.as_str()).or_insert(version.as_str());
        if version_newer(version, slot) {
            *slot = version;
        }
    }
    installed
        .iter()
        .filter_map(|(name, version)| {
            let latest = best.get(name.as_str())?;
            version_newer(latest, version).then(|| GuixPackage {
                name: name.clone(),
                version: Some(version.clone()),
                latest_version: Some(latest.to_string()),
                state: InstallState::Upgradable,
                ..Default::default()
            })
        })
        .collect()
}

/// A package as guix describes it.
#[derive(Debug, Default)]
pub struct GuixPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for GuixPackage {
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
    fn parses_search_records() {
        let stdout = "\
name: ripgrep
version: 14.1.0
outputs: out
systems: x86_64-linux i686-linux
dependencies: rust-bstr@1.6.0 rust-grep@0.2.12
location: gnu/packages/rust-apps.scm:212:2
homepage: https://github.com/BurntSushi/ripgrep
license: Unlicense, Expat
synopsis: Line-oriented search tool that respects your gitignore and
+ searches compressed files
description: ripgrep is a line-oriented search tool.
relevance: 20

name: grep
version: 3.11
homepage: https://www.gnu.org/software/grep/
license: GPL 3+
synopsis: Print lines matching a pattern
relevance: 10
";
        let packages = parse_records(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "ripgrep");
        assert_eq!(packages[0].version.as_deref(), Some("14.1.0"));
        assert_eq!(
            packages[0].description.as_deref(),
            Some("Line-oriented search tool that respects your gitignore and searches compressed files")
        );
        assert_eq!(packages[0].license.as_deref(), Some("Unlicense, Expat"));
        assert_eq!(
            packages[0].dependencies,
            Some(vec!["rust-bstr".to_string(), "rust-grep".to_string()])
        );
        assert_eq!(packages[1].name, "grep");
        assert_eq!(packages[1].state, InstallState::Unknown);
    }

    #[test]
    fn parses_tab_separated_columns() {
        let stdout = "\
ripgrep\t14.1.0\tout\t/gnu/store/abc-ripgrep-14.1.0
hello\t2.12.1\tout\t/gnu/store/def-hello-2.12.1
";
        let columns = parse_columns(stdout);
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0], ("ripgrep".to_string(), "14.1.0".to_string()));
    }

    #[test]
    fn compares_versions() {
        assert!(version_newer("14.1.0", "13.0.0"));
        assert!(version_newer("1.10", "1.9"));
        assert!(!version_newer("1.9", "1.10"));
        assert!(version_newer("2.12.1", "2.12"));
        assert!(!version_newer("14.1.0", "14.1.0"));
    }

    #[test]
    fn diffs_installed_against_available() {
        let installed = vec![
            ("ripgrep".to_string(), "13.0.0".to_string()),
            ("hello".to_string(), "2.12.1".to_string()),
        ];
        let available = vec![
            ("ripgrep".to_string(), "14.1.0".to_string()),
            ("ripgrep".to_string(), "12.1.1".to_string()),
            ("hello".to_string(), "2.12.1".to_string()),
        ];
        let packages = outdated(&installed, &available);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "ripgrep");
        assert_eq!(packages[0].version.as_deref(), Some("13.0.0"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("14.1.0"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(spec(&PackageRequest::parse("gcc@10.3.0")), "gcc@10.3.0");
        assert_eq!(spec(&PackageRequest::parse("ripgrep")), "ripgrep");
    }
}
