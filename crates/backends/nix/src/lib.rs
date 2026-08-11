//! Nix backend for snowcone.
//!
//! Drives the modern `nix profile` / `nix search` CLI against the `nixpkgs`
//! flake, forcing `--extra-experimental-features "nix-command flakes"` on
//! every invocation so stock installs work without nix.conf edits. Profiles
//! are per-user, so nothing elevates. There is no index to refresh and no
//! cheap outdated listing, so those capabilities are not advertised. No
//! `nix profile` mutation has a dry-run (the flag is still an open upstream
//! request, NixOS/nix#7227), so `--dry-run` errors for all of them.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "nix";
const PROGRAMS: &[&str] = &["nix"];
const EXPERIMENTAL: [&str; 2] = ["--extra-experimental-features", "nix-command flakes"];

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
        Cmd::new(&self.program).args(EXPERIMENTAL)
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

    /// The current profile's contents, from `nix profile list --json`.
    async fn profile(&self) -> Result<Vec<NixPackage>> {
        let output = self
            .cmd()
            .args(["profile", "list", "--json"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        let json: Value = serde_json::from_str(&output.stdout).map_err(|error| Error::Parse {
            what: format!("{ID} profile list"),
            detail: error.to_string(),
        })?;
        Ok(parse_profile(&json))
    }

    /// `nix search nixpkgs … --json`, mapping "no results" to an empty set.
    async fn search_nixpkgs(&self, pattern: &str) -> Result<Vec<NixPackage>> {
        let output = self
            .cmd()
            .args(["search", "nixpkgs"])
            .arg(pattern)
            .arg("--json")
            .capture(&self.elevator, None)
            .await?;
        if !output.success() && output.stderr.contains("no results") {
            return Ok(Vec::new());
        }
        let output = output.require_success()?;
        let json: Value = serde_json::from_str(&output.stdout).map_err(|error| Error::Parse {
            what: format!("{ID} search output"),
            detail: error.to_string(),
        })?;
        Ok(parse_search(&json))
    }
}

/// Nixpkgs installs track the flake head; a pinned version has no
/// installable name to resolve to.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but nixpkgs installs track the channel head"
        ))),
        None => Ok(()),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Nix"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Universal
    }

    fn database_id(&self) -> &'static str {
        "nix"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::SEARCH | Capabilities::UPGRADE
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            // `nix profile install` has no --dry-run flag (upstream request
            // NixOS/nix#7227 is still open).
            return Err(Error::Other(format!("{ID}: install has no dry-run mode")));
        }
        let cmd = self.cmd().args(["profile", "install"]).args(
            packages
                .iter()
                .map(|package| format!("nixpkgs#{}", package.name)),
        );
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: remove has no dry-run mode")));
        }
        let cmd = self
            .cmd()
            .args(["profile", "remove"])
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .profile()
            .await?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let pattern = format!("^{}$", regex_escape(name));
        let matches = self.search_nixpkgs(&pattern).await?;
        let mut package = matches
            .into_iter()
            .find(|package| package.name == name)
            .ok_or_else(|| Error::NotFound(name.to_string()))?;
        // The search index says nothing about the local profile; one list
        // fills in the installed state and version.
        if let Some(installed) = self
            .profile()
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
        Ok(self
            .search_nixpkgs(query)
            .await?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            // `nix profile upgrade` has no --dry-run flag either
            // (NixOS/nix#7227).
            return Err(Error::Other(format!("{ID}: upgrade has no dry-run mode")));
        }
        let mut cmd = self.cmd().args(["profile", "upgrade"]);
        if packages.is_empty() {
            cmd = cmd.arg("--all");
        } else {
            cmd = cmd.args(packages.iter().map(|package| package.name.as_str()));
        }
        self.run(cmd, ctx).await
    }
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

/// `/nix/store/<hash>-name-1.2.3` → `("name", Some("1.2.3"))`, using nix's
/// own convention that the version starts at the first dash followed by a
/// digit.
fn parse_store_path(path: &str) -> Option<(String, Option<String>)> {
    let base = path.rsplit('/').next()?;
    let rest = base.split_once('-')?.1;
    for (idx, _) in rest.match_indices('-') {
        if rest[idx + 1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
        {
            return Some((rest[..idx].to_string(), Some(rest[idx + 1..].to_string())));
        }
    }
    Some((rest.to_string(), None))
}

/// `nix profile list --json`: `elements` is a name→element map on current
/// Nix, an array on older versions.
fn parse_profile(json: &Value) -> Vec<NixPackage> {
    let mut packages = Vec::new();
    let mut add = |name: Option<&str>, element: &Value| {
        let store = element["storePaths"][0].as_str().and_then(parse_store_path);
        let attr_name = element["attrPath"]
            .as_str()
            .and_then(|attr| attr.rsplit('.').next());
        let Some(name) = name
            .or(attr_name)
            .map(str::to_string)
            .or_else(|| store.as_ref().map(|(name, _)| name.clone()))
        else {
            return;
        };
        packages.push(NixPackage {
            name,
            version: store.and_then(|(_, version)| version),
            origin: element["originalUrl"].as_str().map(str::to_string),
            state: InstallState::Installed,
            ..Default::default()
        });
    };
    if let Some(map) = json["elements"].as_object() {
        for (name, element) in map {
            add(Some(name), element);
        }
    } else if let Some(items) = json["elements"].as_array() {
        for element in items {
            add(None, element);
        }
    }
    packages
}

/// `nix search --json`: attribute path keys mapping to pname/version/
/// description objects.
fn parse_search(json: &Value) -> Vec<NixPackage> {
    let Some(map) = json.as_object() else {
        return Vec::new();
    };
    map.iter()
        .map(|(attr, entry)| {
            let pname = entry["pname"]
                .as_str()
                .map(str::to_string)
                .or_else(|| attr.rsplit('.').next().map(str::to_string))
                .unwrap_or_else(|| attr.clone());
            NixPackage {
                name: pname,
                version: entry["version"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .map(str::to_string),
                description: entry["description"]
                    .as_str()
                    .filter(|d| !d.is_empty())
                    .map(str::to_string),
                origin: Some("nixpkgs".to_string()),
                state: InstallState::Available,
                ..Default::default()
            }
        })
        .collect()
}

/// A package as nix describes it.
#[derive(Debug, Default)]
pub struct NixPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub origin: Option<String>,
    pub state: InstallState,
}

impl Package for NixPackage {
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
    fn splits_store_paths_into_name_and_version() {
        assert_eq!(
            parse_store_path("/nix/store/abc123-ripgrep-14.1.0"),
            Some(("ripgrep".to_string(), Some("14.1.0".to_string())))
        );
        assert_eq!(
            parse_store_path("/nix/store/abc123-nixpkgs-review"),
            Some(("nixpkgs-review".to_string(), None))
        );
    }

    #[test]
    fn parses_profile_element_map() {
        let json: Value = serde_json::from_str(
            r#"{"elements": {"ripgrep": {
                "active": true,
                "attrPath": "legacyPackages.x86_64-linux.ripgrep",
                "originalUrl": "flake:nixpkgs",
                "storePaths": ["/nix/store/abc123-ripgrep-14.1.0"]
            }}, "version": 2}"#,
        )
        .unwrap();
        let packages = parse_profile(&json);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "ripgrep");
        assert_eq!(packages[0].version.as_deref(), Some("14.1.0"));
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn parses_profile_element_array() {
        let json: Value = serde_json::from_str(
            r#"{"elements": [{
                "attrPath": "legacyPackages.x86_64-linux.hello",
                "storePaths": ["/nix/store/abc123-hello-2.12.1"]
            }]}"#,
        )
        .unwrap();
        let packages = parse_profile(&json);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "hello");
        assert_eq!(packages[0].version.as_deref(), Some("2.12.1"));
    }

    #[test]
    fn parses_search_results() {
        let json: Value = serde_json::from_str(
            r#"{"legacyPackages.x86_64-linux.ripgrep": {
                "pname": "ripgrep",
                "version": "14.1.0",
                "description": "Utility that combines the usability of ag with the raw speed of grep"
            }}"#,
        )
        .unwrap();
        let packages = parse_search(&json);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "ripgrep");
        assert_eq!(packages[0].origin.as_deref(), Some("nixpkgs"));
    }

    #[test]
    fn escapes_regex_metacharacters() {
        assert_eq!(regex_escape("gtk+"), "gtk\\+");
        assert_eq!(regex_escape("ripgrep"), "ripgrep");
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("ripgrep@14.1.0")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("ripgrep")]).is_ok());
    }
}
