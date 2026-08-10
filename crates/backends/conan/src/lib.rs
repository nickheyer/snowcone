//! Conan 2 backend for snowcone.
//!
//! Packages are recipes with at least one binary in Conan's local cache.
//! Reads use Conan's supported JSON formatter; remote search spans every
//! configured remote. Unpinned installs use an open version range, which is
//! Conan's native mechanism for selecting the newest matching recipe.

use std::collections::BTreeSet;
use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "conan";
const PROGRAMS: &[&str] = &["conan"];

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
        Cmd::new(&self.program)
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

    async fn list(&self, pattern: &str, remotes: bool) -> Result<Vec<ConanPackage>> {
        let mut cmd = self.query().args(["list", pattern, "--format=json"]);
        if remotes {
            cmd = cmd.arg("--remote=*");
        }
        let output = cmd.capture(&self.elevator, None).await?.require_success()?;
        parse_list_json(
            &output.stdout,
            if remotes {
                InstallState::Available
            } else {
                InstallState::Installed
            },
        )
    }
}

fn install_ref(package: &PackageRequest) -> String {
    match &package.version {
        Some(version) => format!("{}/{version}", package.name),
        None if package.name.contains('/') => package.name.clone(),
        // Conan resolves ranges to the newest compatible recipe.
        None => format!("{}/[>=0]", package.name),
    }
}

fn remove_pattern(package: &PackageRequest) -> String {
    match &package.version {
        Some(version) => format!("{}/{version}", package.name),
        None if package.name.contains('/') => package.name.clone(),
        None => format!("{}/*", package.name),
    }
}

fn search_pattern(query: &str) -> String {
    if query.contains('/') {
        query.to_string()
    } else {
        format!("*{query}*/*")
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Conan"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "conan"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::SEARCH | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: install has no dry-run mode")));
        }
        let cmd = self.cmd().arg("install").args(
            packages
                .iter()
                .map(|package| format!("--requires={}", install_ref(package))),
        );
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        // `conan remove` accepts one pattern per invocation.
        for package in packages {
            let mut cmd = self
                .cmd()
                .args(["remove", &remove_pattern(package), "--confirm"]);
            if ctx.dry_run {
                cmd = cmd.arg("--dry-run");
            }
            self.run(cmd, ctx).await?;
        }
        Ok(())
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.list("*/*:*", false).await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let local_pattern = if name.contains('/') {
            format!("{name}:*")
        } else {
            format!("{name}/*:*")
        };
        let mut packages = self.list(&local_pattern, false).await?;
        if packages.is_empty() {
            let remote_pattern = if name.contains('/') {
                name.to_string()
            } else {
                format!("{name}/*")
            };
            packages = self.list(&remote_pattern, true).await?;
        }
        packages
            .into_iter()
            .next()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.into()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.list(&search_pattern(query), true).await?))
    }
}

fn parse_list_json(stdout: &str, state: InstallState) -> Result<Vec<ConanPackage>> {
    let root: Value = serde_json::from_str(stdout).map_err(|error| Error::Parse {
        what: format!("{ID} list JSON"),
        detail: error.to_string(),
    })?;
    let sources = root.as_object().ok_or_else(|| Error::Parse {
        what: format!("{ID} list JSON"),
        detail: "top-level value is not an object".into(),
    })?;
    let mut references = BTreeSet::new();
    for source in sources.values().filter_map(Value::as_object) {
        references.extend(source.keys().cloned());
    }
    Ok(references
        .into_iter()
        .filter_map(|reference| parse_reference(&reference, state))
        .collect())
}

fn parse_reference(reference: &str, state: InstallState) -> Option<ConanPackage> {
    let reference = reference.split('#').next()?;
    let (name, version) = reference.split_once('/')?;
    Some(ConanPackage {
        name: name.to_string(),
        version: Some(version.to_string()),
        description: None,
        state,
    })
}

fn boxed(packages: Vec<ConanPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// A Conan recipe reference present in a cache or configured remote.
#[derive(Debug)]
pub struct ConanPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for ConanPackage {
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
    fn formats_refs_and_patterns() {
        assert_eq!(
            install_ref(&PackageRequest::parse("zlib@1.3.1")),
            "zlib/1.3.1"
        );
        assert_eq!(install_ref(&PackageRequest::parse("zlib")), "zlib/[>=0]");
        assert_eq!(remove_pattern(&PackageRequest::parse("zlib")), "zlib/*");
        assert_eq!(search_pattern("zip"), "*zip*/*");
    }

    #[test]
    fn parses_all_json_sources_and_channels() {
        let json = r#"{
          "Local Cache": {"zlib/1.3.1": {"revisions": {}}},
          "private": {"hello/2.0@acme/stable": {"revisions": {}}}
        }"#;
        let packages = parse_list_json(json, InstallState::Installed).unwrap();
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "hello");
        assert_eq!(packages[0].version.as_deref(), Some("2.0@acme/stable"));
        assert_eq!(packages[1].name, "zlib");
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(parse_list_json("[]", InstallState::Installed).is_err());
    }
}
