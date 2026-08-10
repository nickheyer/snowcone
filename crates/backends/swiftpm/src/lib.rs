//! Swift Package Manager backend for snowcone.
//!
//! This backend operates on the current package's `Package.swift` and
//! `Package.resolved`. SwiftPM's manifest refactoring command adds new
//! dependencies, its resolver fetches them, and the resolved dependency graph
//! supplies concrete package metadata. SwiftPM has no dependency-removal
//! command, so removal is rejected rather than rewriting Swift source with an
//! incomplete parser.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::Value;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "swiftpm";
const PROGRAMS: &[&str] = &["swift"];

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
        let program = find_program(PROGRAMS[0]).ok_or_else(|| Error::Unavailable(ID.into()))?;
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
        Cmd::new(&self.program).arg("package")
    }

    fn query(&self) -> Cmd {
        self.cmd().env("LC_ALL", "C")
    }

    async fn run(&self, cmd: Cmd, ctx: &OpContext) -> Result<()> {
        let output = match &ctx.events {
            Some(events) => cmd.capture(&self.elevator, Some(events)).await?,
            None => cmd.run_interactive(&self.elevator).await?,
        };
        output.require_success()?;
        Ok(())
    }

    async fn resolved(&self) -> Result<Vec<SwiftpmPackage>> {
        let output = self
            .query()
            .args(["show-dependencies", "--format", "json"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        parse_dependency_graph(&output.stdout)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DependencyKind {
    Url,
    Registry,
    Path,
}

fn dependency_kind(dependency: &str) -> DependencyKind {
    let path = Path::new(dependency);
    if path.is_absolute() || dependency.starts_with("./") || dependency.starts_with("../") {
        DependencyKind::Path
    } else if !dependency.contains(['/', ':'])
        && dependency
            .split('.')
            .filter(|part| !part.is_empty())
            .count()
            == 2
    {
        DependencyKind::Registry
    } else {
        DependencyKind::Url
    }
}

fn validate_install_request(package: &PackageRequest) -> Result<DependencyKind> {
    let kind = dependency_kind(&package.name);
    match (kind, package.version.as_deref()) {
        (DependencyKind::Path, Some(_)) => Err(Error::Other(format!(
            "{ID}: local dependency `{}` cannot have a version",
            package.name
        ))),
        (DependencyKind::Url | DependencyKind::Registry, None) => Err(Error::Other(format!(
            "{ID}: `{}` requires an exact version (use package@version); SwiftPM's add-dependency command requires an explicit requirement",
            package.name
        ))),
        _ => Ok(kind),
    }
}

fn reject_upgrade_versions(packages: &[PackageRequest]) -> Result<()> {
    if let Some(package) = packages.iter().find(|package| package.version.is_some()) {
        Err(Error::Other(format!(
            "{ID}: `{package}` cannot change its requirement during update; Package.swift owns the version requirement"
        )))
    } else {
        Ok(())
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Swift Package Manager"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "swiftpm"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: install has no dry-run mode")));
        }
        let kinds = packages
            .iter()
            .map(validate_install_request)
            .collect::<Result<Vec<_>>>()?;
        for (package, kind) in packages.iter().zip(kinds) {
            let mut cmd = self.cmd().arg("add-dependency").arg(&package.name);
            match kind {
                DependencyKind::Path => {
                    cmd = cmd.args(["--type", "path"]);
                }
                DependencyKind::Registry => {
                    cmd = cmd.args(["--type", "registry"]);
                    if let Some(version) = &package.version {
                        cmd = cmd.arg("--exact").arg(version);
                    }
                }
                DependencyKind::Url => {
                    if let Some(version) = &package.version {
                        cmd = cmd.arg("--exact").arg(version);
                    }
                }
            }
            self.run(cmd, ctx).await?;
        }
        self.run(self.cmd().arg("resolve"), ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        let target = packages
            .first()
            .map_or("a dependency", |package| package.name.as_str());
        Err(Error::Other(format!(
            "{ID}: SwiftPM has no command to remove `{target}`; remove its .package(...) declaration from Package.swift, then run `swift package resolve`"
        )))
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.resolved().await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        self.resolved()
            .await?
            .into_iter()
            .find(|package| {
                package.identity.eq_ignore_ascii_case(name)
                    || package.display_name.eq_ignore_ascii_case(name)
            })
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.into()))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_upgrade_versions(packages)?;
        let mut cmd = self
            .cmd()
            .arg("update")
            .args(packages.iter().map(|package| package.name.as_str()));
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["update", "--dry-run"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        let mut graph = self.resolved().await?;
        let changes = parse_dry_run_updates(&output.stdout);
        let mut packages = Vec::new();
        for (identity, current, latest) in changes {
            let mut package = graph
                .iter()
                .position(|package| package.identity.eq_ignore_ascii_case(&identity))
                .map(|index| graph.swap_remove(index))
                .unwrap_or_else(|| SwiftpmPackage {
                    identity: identity.clone(),
                    display_name: identity,
                    version: Some(current.clone()),
                    latest_version: None,
                    description: None,
                    url: None,
                    path: None,
                    traits: Vec::new(),
                    dependencies: Vec::new(),
                    state: InstallState::Installed,
                });
            package.version = Some(current);
            package.latest_version = Some(latest);
            package.state = InstallState::Upgradable;
            packages.push(package);
        }
        Ok(boxed(packages))
    }
}

fn parse_dependency_graph(stdout: &str) -> Result<Vec<SwiftpmPackage>> {
    let root: Value = serde_json::from_str(stdout).map_err(|error| Error::Parse {
        what: format!("{ID} dependency graph JSON"),
        detail: error.to_string(),
    })?;
    let dependencies = root
        .get("dependencies")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Parse {
            what: format!("{ID} dependency graph JSON"),
            detail: "root is missing a dependencies array".into(),
        })?;
    let mut seen = HashSet::new();
    let mut packages = Vec::new();
    for dependency in dependencies {
        collect_dependency(dependency, &mut seen, &mut packages)?;
    }
    Ok(packages)
}

fn collect_dependency(
    node: &Value,
    seen: &mut HashSet<String>,
    packages: &mut Vec<SwiftpmPackage>,
) -> Result<()> {
    let object = node.as_object().ok_or_else(|| Error::Parse {
        what: format!("{ID} dependency graph JSON"),
        detail: "dependency is not an object".into(),
    })?;
    let identity = object
        .get("identity")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Parse {
            what: format!("{ID} dependency graph JSON"),
            detail: "dependency is missing identity".into(),
        })?
        .to_string();
    let children = object
        .get("dependencies")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Parse {
            what: format!("{ID} dependency graph JSON"),
            detail: format!("dependency `{identity}` is missing dependencies"),
        })?;
    let dependencies = children
        .iter()
        .filter_map(|child| child.get("identity").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    if seen.insert(identity.clone()) {
        let display_name = object
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&identity)
            .to_string();
        let version = object
            .get("version")
            .and_then(Value::as_str)
            .filter(|version| *version != "unspecified")
            .map(str::to_string);
        let traits = object
            .get("traits")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        packages.push(SwiftpmPackage {
            identity,
            description: (display_name != object["identity"].as_str().unwrap_or_default())
                .then(|| display_name.clone()),
            display_name,
            version,
            latest_version: None,
            url: object
                .get("url")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())
                .map(str::to_string),
            path: object
                .get("path")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
            traits,
            dependencies,
            state: InstallState::Installed,
        });
    }
    for child in children {
        collect_dependency(child, seen, packages)?;
    }
    Ok(())
}

fn parse_dry_run_updates(stdout: &str) -> Vec<(String, String, String)> {
    stdout
        .lines()
        .filter_map(|line| {
            let change = line.trim().strip_prefix("~ ")?;
            let (before, after) = change.split_once(" -> ")?;
            let (identity, current) = before.split_once(' ')?;
            let (new_identity, latest) = after.split_once(' ')?;
            identity.eq_ignore_ascii_case(new_identity).then(|| {
                (
                    identity.to_string(),
                    current.to_string(),
                    latest.to_string(),
                )
            })
        })
        .collect()
}

fn boxed(packages: Vec<SwiftpmPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// A dependency in SwiftPM's resolved graph for the current package.
#[derive(Debug)]
pub struct SwiftpmPackage {
    pub identity: String,
    pub display_name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub url: Option<String>,
    pub path: Option<PathBuf>,
    pub traits: Vec<String>,
    pub dependencies: Vec<String>,
    pub state: InstallState,
}

impl Package for SwiftpmPackage {
    fn manager(&self) -> &str {
        ID
    }

    fn name(&self) -> &str {
        &self.identity
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
        self.url
            .as_deref()
            .or_else(|| self.path.as_deref().and_then(|path| path.to_str()))
    }

    fn dependencies(&self) -> Option<Vec<String>> {
        Some(self.dependencies.clone())
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_dependency_inputs_and_requires_remote_versions() {
        assert_eq!(dependency_kind("../LocalKit"), DependencyKind::Path);
        assert_eq!(dependency_kind("mona.LinkedList"), DependencyKind::Registry);
        assert_eq!(
            dependency_kind("https://github.com/apple/swift-argument-parser.git"),
            DependencyKind::Url
        );
        assert!(validate_install_request(&PackageRequest::parse("mona.LinkedList")).is_err());
        assert!(validate_install_request(&PackageRequest::parse("mona.LinkedList@1.2.0")).is_ok());
    }

    #[test]
    fn parses_recursive_graph_and_deduplicates_packages() {
        let json = r#"{
          "identity":"demo","name":"Demo","url":"/demo","version":"unspecified","path":"/demo","traits":[],
          "dependencies":[
            {"identity":"argument-parser","name":"swift-argument-parser","url":"https://github.com/apple/swift-argument-parser.git","version":"1.5.0","path":"/.build/checkouts/swift-argument-parser","traits":["Parser"],"dependencies":[
              {"identity":"system","name":"swift-system","url":"https://github.com/apple/swift-system.git","version":"1.4.0","path":"/.build/checkouts/swift-system","traits":[],"dependencies":[]}
            ]},
            {"identity":"system","name":"swift-system","url":"https://github.com/apple/swift-system.git","version":"1.4.0","path":"/.build/checkouts/swift-system","traits":[],"dependencies":[]}
          ]
        }"#;
        let packages = parse_dependency_graph(json).unwrap();
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].identity, "argument-parser");
        assert_eq!(packages[0].dependencies, vec!["system"]);
        assert_eq!(packages[0].traits, vec!["Parser"]);
        assert_eq!(packages[1].version.as_deref(), Some("1.4.0"));
    }

    #[test]
    fn parses_update_dry_run() {
        let output = "[Dry-run] 2 dependencies would change:\n~ argument-parser 1.4.0 -> argument-parser 1.5.0\n+ logging 1.6.0\n";
        let updates = parse_dry_run_updates(output);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0, "argument-parser");
        assert_eq!(updates[0].2, "1.5.0");
    }
}
