//! Luet backend for snowcone.
//!
//! Luet (MocaccinoOS) is a container-built system manager whose packages
//! are addressed as `category/name`. Every read goes through `luet search
//! --output json` - the one machine-readable window luet offers - with
//! `--installed` flipping it from the repositories to the local database,
//! so no locale pinning is needed. `--yes` pre-answers confirmations;
//! there is no dry-run flag, upgrades are whole-system only, and the
//! outdated listing is computed by joining the installed set against the
//! repositories.

use std::cmp::Ordering;
use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "luet";
const PROGRAMS: &[&str] = &["luet"];

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

    fn no_dry_run(&self, operation: &str) -> Error {
        Error::Other(format!("{ID}: {operation} has no dry-run mode"))
    }

    /// Shared flags for mutating commands: `--yes` when confirmations are
    /// pre-answered.
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(subcommand).elevated(true);
        if ctx.assume_yes {
            cmd = cmd.arg("--yes");
        }
        cmd
    }

    /// One `luet search --output json` invocation, parsed into packages
    /// carrying the given state.
    async fn search_json(&self, args: &[&str], state: InstallState) -> Result<Vec<LuetPackage>> {
        let output = self
            .cmd()
            .arg("search")
            .args(args.iter().copied())
            .args(["--output", "json"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        if output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        let json: Value = serde_json::from_str(output.stdout.trim()).map_err(|error| {
            Error::Parse {
                what: format!("{ID} search output"),
                detail: error.to_string(),
            }
        })?;
        Ok(parse_search(&json, state))
    }
}

/// Luet resolves versions itself; requests must not carry one.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but luet installs the repositories' current version"
        ))),
        None => Ok(()),
    }
}

/// The `name` half of a `category/name` spec (the whole spec when it has
/// no category).
fn name_part(spec: &str) -> &str {
    spec.rsplit('/').next().unwrap_or(spec)
}

/// Anchor a package name inside a regex without letting `+`, `.`, … act as
/// metacharacters.
fn regex_escape(name: &str) -> String {
    let mut escaped = String::with_capacity(name.len());
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() && c != '_' && c != '-' {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Luet"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "luet"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::REFRESH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
    }

    fn needs_elevation(&self, operation: Operation) -> bool {
        matches!(
            operation,
            Operation::Install | Operation::Remove | Operation::Upgrade | Operation::Refresh
        )
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        let cmd = self
            .mutation("install", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("uninstall"));
        }
        let cmd = self
            .mutation("uninstall", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(
            self.search_json(&["--installed", "."], InstallState::Installed)
                .await?,
        ))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let pattern = regex_escape(name_part(name));
        let available = self
            .search_json(&[&pattern], InstallState::Available)
            .await?;
        let installed = self
            .search_json(&["--installed", &pattern], InstallState::Installed)
            .await?;
        // A bare name matches any category; a `category/name` spec must
        // match exactly.
        let matches_request =
            |package: &LuetPackage| package.name == name || name_part(&package.name) == name;
        let repo = available.into_iter().find(|p| matches_request(p));
        let local = installed.into_iter().find(|p| matches_request(p));
        let package = match (repo, local) {
            (Some(mut repo), Some(local)) => {
                repo.state = InstallState::Installed;
                if let (Some(local_version), Some(repo_version)) =
                    (local.version.clone(), repo.version.clone())
                    && local_version != repo_version
                {
                    if version_newer(&repo_version, &local_version) {
                        repo.state = InstallState::Upgradable;
                    }
                    repo.latest_version = Some(repo_version);
                    repo.version = Some(local_version);
                }
                repo
            }
            (Some(repo), None) => repo,
            (None, Some(local)) => local,
            (None, None) => return Err(Error::NotFound(name.to_string())),
        };
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(
            self.search_json(&[query], InstallState::Available).await?,
        ))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("refresh"));
        }
        self.run(self.cmd().args(["repo", "update"]).elevated(true), ctx)
            .await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        if !packages.is_empty() {
            return Err(Error::Other(format!(
                "{ID}: upgrade works on the whole system only; luet has no per-package upgrade"
            )));
        }
        self.run(self.mutation("upgrade", ctx), ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        // luet has no outdated verb; join the installed set against the
        // repositories and keep what the repos hold a newer version of.
        let installed = self
            .search_json(&["--installed", "."], InstallState::Installed)
            .await?;
        let available = self.search_json(&["."], InstallState::Available).await?;
        Ok(boxed(outdated(installed, &available)))
    }
}

fn boxed(packages: Vec<LuetPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `luet search --output json`: `{"packages": [...]}` on current builds,
/// `{"stones": [...]}` or a bare array on older ones; entries carry
/// `category`, `name`, `version`, and sometimes `repository`.
fn parse_search(json: &Value, state: InstallState) -> Vec<LuetPackage> {
    let entries = json
        .as_array()
        .or_else(|| json["packages"].as_array())
        .or_else(|| json["stones"].as_array());
    let Some(entries) = entries else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let name = entry["name"].as_str()?;
            let category = entry["category"].as_str().filter(|c| !c.is_empty());
            Some(LuetPackage {
                name: match category {
                    Some(category) => format!("{category}/{name}"),
                    None => name.to_string(),
                },
                version: entry["version"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .map(str::to_string),
                description: entry["description"]
                    .as_str()
                    .filter(|d| !d.is_empty())
                    .map(str::to_string),
                origin: entry["repository"]
                    .as_str()
                    .filter(|r| !r.is_empty())
                    .map(str::to_string),
                state,
                ..Default::default()
            })
        })
        .collect()
}

/// Join installed packages against repository entries, keeping those where
/// some repository version is strictly newer.
fn outdated(installed: Vec<LuetPackage>, available: &[LuetPackage]) -> Vec<LuetPackage> {
    installed
        .into_iter()
        .filter_map(|mut package| {
            let current = package.version.clone()?;
            let newest = available
                .iter()
                .filter(|candidate| candidate.name == package.name)
                .filter_map(|candidate| candidate.version.as_deref())
                .filter(|candidate| version_newer(candidate, &current))
                .max_by(|a, b| compare_versions(a, b))?;
            package.latest_version = Some(newest.to_string());
            package.state = InstallState::Upgradable;
            Some(package)
        })
        .collect()
}

/// Order two version strings segment-by-segment (split on `.`, `-`, `_`,
/// `+`), numerically where both segments are numbers, lexically otherwise;
/// extra trailing segments sort newer.
fn compare_versions(a: &str, b: &str) -> Ordering {
    let mut left = a.split(['.', '-', '_', '+']);
    let mut right = b.split(['.', '-', '_', '+']);
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(l), Some(r)) => {
                let ordering = match (l.parse::<u64>(), r.parse::<u64>()) {
                    (Ok(l), Ok(r)) => l.cmp(&r),
                    _ => l.cmp(r),
                };
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
        }
    }
}

fn version_newer(candidate: &str, current: &str) -> bool {
    compare_versions(candidate, current) == Ordering::Greater
}

/// A package as luet describes it, named `category/name`.
#[derive(Debug, Default)]
pub struct LuetPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub origin: Option<String>,
    pub state: InstallState,
}

impl Package for LuetPackage {
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

    fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_packages_object() {
        let json: Value = serde_json::from_str(
            r#"{"packages": [{
                "name": "bash",
                "category": "system",
                "version": "5.2",
                "repository": "mocaccino-repository-index",
                "description": "The GNU Bourne Again SHell"
            }]}"#,
        )
        .unwrap();
        let packages = parse_search(&json, InstallState::Available);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "system/bash");
        assert_eq!(packages[0].version.as_deref(), Some("5.2"));
        assert_eq!(
            packages[0].origin.as_deref(),
            Some("mocaccino-repository-index")
        );
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn parses_stones_object_and_bare_array() {
        let stones: Value = serde_json::from_str(
            r#"{"stones": [{"name": "zsh", "category": "shells", "version": "5.9"}]}"#,
        )
        .unwrap();
        assert_eq!(
            parse_search(&stones, InstallState::Installed)[0].name,
            "shells/zsh"
        );
        let array: Value =
            serde_json::from_str(r#"[{"name": "zsh", "category": "", "version": "5.9"}]"#).unwrap();
        let packages = parse_search(&array, InstallState::Installed);
        assert_eq!(packages[0].name, "zsh");
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn compares_versions_numerically() {
        assert!(version_newer("1.10.0", "1.9.0"));
        assert!(version_newer("1.2.1", "1.2"));
        assert!(!version_newer("1.2.3", "1.2.3"));
        assert!(!version_newer("1.2.3", "2.0"));
    }

    #[test]
    fn joins_outdated_against_repositories() {
        let installed = vec![
            LuetPackage {
                name: "system/bash".to_string(),
                version: Some("5.1".to_string()),
                state: InstallState::Installed,
                ..Default::default()
            },
            LuetPackage {
                name: "shells/zsh".to_string(),
                version: Some("5.9".to_string()),
                state: InstallState::Installed,
                ..Default::default()
            },
        ];
        let available = vec![
            LuetPackage {
                name: "system/bash".to_string(),
                version: Some("5.2".to_string()),
                ..Default::default()
            },
            LuetPackage {
                name: "system/bash".to_string(),
                version: Some("5.0".to_string()),
                ..Default::default()
            },
            LuetPackage {
                name: "shells/zsh".to_string(),
                version: Some("5.9".to_string()),
                ..Default::default()
            },
        ];
        let outdated = outdated(installed, &available);
        assert_eq!(outdated.len(), 1);
        assert_eq!(outdated[0].name, "system/bash");
        assert_eq!(outdated[0].version.as_deref(), Some("5.1"));
        assert_eq!(outdated[0].latest_version.as_deref(), Some("5.2"));
        assert_eq!(outdated[0].state, InstallState::Upgradable);
    }

    #[test]
    fn splits_specs_and_escapes_regex() {
        assert_eq!(name_part("system/bash"), "bash");
        assert_eq!(name_part("bash"), "bash");
        assert_eq!(regex_escape("gtk+"), "gtk\\+");
        assert_eq!(regex_escape("bash"), "bash");
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("system/bash@5.2")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("system/bash")]).is_ok());
    }
}
