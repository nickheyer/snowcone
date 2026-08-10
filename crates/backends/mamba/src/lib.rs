//! Mamba backend for snowcone.
//!
//! Drives `mamba` (or `micromamba`) against the `base` environment
//! explicitly (`-n base`): snowcone is a system-wide package view, so the
//! one environment that always exists is the one it manages. Parsed reads
//! use `--json` (locale-proof); mutations run without it so the tool's own
//! prompts and progress bars survive passthrough. `search --json` has two
//! shapes in the wild: mamba 1.x forwarded search to conda (a name → builds
//! map, with "no match" as a nonzero exit carrying a JSON
//! `PackagesNotFoundError` body), while mamba 2.x and micromamba answer with
//! the repoquery shape (`result.pkgs`) - both are parsed. No outdated verb
//! exists, so `list_outdated` parses the `update --all --dry-run --json`
//! plan. Environments are per-user - nothing elevates.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "mamba";
const PROGRAMS: &[&str] = &["mamba", "micromamba"];

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
                reason: format!("none of {PROGRAMS:?} found on PATH"),
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

    /// Shared flags for mutating commands: the base env, `-y`, and the
    /// native dry-run switch.
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(subcommand).args(["-n", "base"]);
        if ctx.assume_yes {
            cmd = cmd.arg("-y");
        }
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        cmd
    }

    /// The base environment's contents, from `mamba list -n base --json`.
    async fn installed(&self) -> Result<Vec<MambaPackage>> {
        let output = self
            .cmd()
            .args(["list", "-n", "base", "--json"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_list(&parse_json("list output", &output.stdout)?))
    }

    /// `mamba search … --json`, tolerating both "no match" conventions: the
    /// conda-forwarded PackagesNotFoundError body (nonzero exit) and the
    /// repoquery shape's empty `result.pkgs`.
    async fn search_channels(&self, pattern: &str) -> Result<Vec<MambaPackage>> {
        let output = self
            .cmd()
            .arg("search")
            .arg(pattern)
            .arg("--json")
            .capture(&self.elevator, None)
            .await?;
        let Ok(json) = serde_json::from_str::<Value>(&output.stdout) else {
            output.require_success()?;
            return Ok(Vec::new());
        };
        if json["exception_name"].as_str() == Some("PackagesNotFoundError") {
            return Ok(Vec::new());
        }
        if !output.success() && !json["result"].is_object() {
            output.require_success()?;
        }
        Ok(parse_search(&json))
    }
}

/// `name=version` when the request pins one, bare name otherwise.
fn spec(request: &PackageRequest) -> String {
    match &request.version {
        Some(version) => format!("{}={version}", request.name),
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
        "Mamba"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Universal
    }

    fn database_id(&self) -> &'static str {
        "conda"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = self
            .mutation("install", ctx)
            .args(packages.iter().map(spec));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = self
            .mutation("remove", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.installed().await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let mut package = self
            .search_channels(name)
            .await?
            .into_iter()
            .find(|package| package.name == name)
            .ok_or_else(|| Error::NotFound(name.to_string()))?;
        // The channel view says nothing about the base env; one list probe
        // fills in the installed state and version.
        if let Some(installed) = self
            .installed()
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
        // A bare term only matches whole names; wildcards make it a search.
        let pattern = if query.contains('*') {
            query.to_string()
        } else {
            format!("*{query}*")
        };
        Ok(boxed(self.search_channels(&pattern).await?))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if packages.is_empty() {
            return self
                .run(self.mutation("update", ctx).arg("--all"), ctx)
                .await;
        }
        // Moving a package to a pinned version is `install name=ver` in the
        // conda family; `update` only moves to the newest.
        let subcommand = if packages.iter().any(|package| package.version.is_some()) {
            "install"
        } else {
            "update"
        };
        let cmd = self
            .mutation(subcommand, ctx)
            .args(packages.iter().map(spec));
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        // No outdated verb exists; the dry-run plan of `update --all` is the
        // closest honest answer. When everything is current the JSON has no
        // actions.
        let output = self
            .cmd()
            .args(["update", "-n", "base", "--all", "--dry-run", "--json"])
            .capture(&self.elevator, None)
            .await?;
        match serde_json::from_str::<Value>(&output.stdout) {
            Ok(json) => Ok(boxed(parse_outdated(&json))),
            Err(_) => {
                output.require_success()?;
                Ok(Vec::new())
            }
        }
    }
}

fn boxed(packages: Vec<MambaPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `mamba list --json`: an array of installed-record objects
/// (`name`/`version`/`channel`/`platform`).
fn parse_list(json: &Value) -> Vec<MambaPackage> {
    let Some(entries) = json.as_array() else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            Some(MambaPackage {
                name: entry["name"].as_str()?.to_string(),
                version: entry["version"].as_str().map(str::to_string),
                origin: entry["channel"].as_str().map(str::to_string),
                architecture: entry["platform"].as_str().map(str::to_string),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `mamba search --json`: either the repoquery shape (`result.pkgs`, a flat
/// array of build entries - mamba 2.x and micromamba) or the conda-style
/// name → builds map (mamba 1.x). The last build listed per name wins.
fn parse_search(json: &Value) -> Vec<MambaPackage> {
    if let Some(pkgs) = json["result"]["pkgs"].as_array() {
        let mut packages: Vec<MambaPackage> = Vec::new();
        for entry in pkgs {
            let Some(package) = parse_build(entry, None) else {
                continue;
            };
            match packages.iter_mut().find(|seen| seen.name == package.name) {
                Some(seen) => *seen = package,
                None => packages.push(package),
            }
        }
        return packages;
    }
    let Some(map) = json.as_object() else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(name, builds)| parse_build(builds.as_array()?.last()?, Some(name)))
        .collect()
}

/// One repodata build entry (`version`/`channel`/`subdir`/`depends`/…).
fn parse_build(entry: &Value, name: Option<&str>) -> Option<MambaPackage> {
    let name = name.or_else(|| entry["name"].as_str())?;
    Some(MambaPackage {
        name: name.to_string(),
        version: entry["version"].as_str().map(str::to_string),
        license: entry["license"]
            .as_str()
            .filter(|license| !license.is_empty())
            .map(str::to_string),
        architecture: entry["subdir"].as_str().map(str::to_string),
        origin: entry["channel"].as_str().map(str::to_string),
        download_size: entry["size"].as_u64(),
        dependencies: entry["depends"].as_array().map(|deps| {
            deps.iter()
                .filter_map(|dep| dep.as_str()?.split_whitespace().next())
                .map(str::to_string)
                .collect()
        }),
        state: InstallState::Available,
        ..Default::default()
    })
}

/// `mamba update --all --dry-run --json`: planned changes under `actions`,
/// where an UNLINK/LINK pair for one name is an upgrade (UNLINK carries the
/// installed version, LINK the incoming one). LINK-only entries are new
/// dependencies and same-version pairs are build bumps - neither is an
/// outdated package.
fn parse_outdated(json: &Value) -> Vec<MambaPackage> {
    let Some(links) = json["actions"]["LINK"].as_array() else {
        return Vec::new();
    };
    let unlinked: Vec<(&str, &str)> = json["actions"]["UNLINK"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| Some((entry["name"].as_str()?, entry["version"].as_str()?)))
                .collect()
        })
        .unwrap_or_default();
    links
        .iter()
        .filter_map(|entry| {
            let name = entry["name"].as_str()?;
            let latest = entry["version"].as_str()?;
            let (_, current) = unlinked.iter().find(|(unlinked, _)| *unlinked == name)?;
            (*current != latest).then(|| MambaPackage {
                name: name.to_string(),
                version: Some((*current).to_string()),
                latest_version: Some(latest.to_string()),
                origin: entry["channel"].as_str().map(str::to_string),
                state: InstallState::Upgradable,
                ..Default::default()
            })
        })
        .collect()
}

/// A package as mamba describes it.
#[derive(Debug, Default)]
pub struct MambaPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub license: Option<String>,
    pub architecture: Option<String>,
    pub origin: Option<String>,
    pub download_size: Option<u64>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for MambaPackage {
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

    fn license(&self) -> Option<&str> {
        self.license.as_deref()
    }

    fn architecture(&self) -> Option<&str> {
        self.architecture.as_deref()
    }

    fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
    }

    fn download_size(&self) -> Option<u64> {
        self.download_size
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
    fn parses_installed_list() {
        let json: Value = serde_json::from_str(
            r#"[
                {"base_url": "https://conda.anaconda.org/conda-forge",
                 "build_number": 1, "build_string": "h8fae777_1",
                 "channel": "conda-forge", "dist_name": "fd-find-10.2.0-h8fae777_1",
                 "name": "fd-find", "platform": "linux-64", "version": "10.2.0"},
                {"channel": "conda-forge", "name": "bat",
                 "platform": "linux-64", "version": "0.24.0"}
            ]"#,
        )
        .unwrap();
        let packages = parse_list(&json);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "fd-find");
        assert_eq!(packages[0].version.as_deref(), Some("10.2.0"));
        assert_eq!(packages[0].architecture.as_deref(), Some("linux-64"));
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn parses_repoquery_search_shape() {
        let json: Value = serde_json::from_str(
            r#"{"query": {"query": "ripgrep", "type": "search"},
                "result": {"pkgs": [
                    {"build": "hc07d326_0", "channel": "conda-forge",
                     "depends": ["libgcc-ng >=9"], "license": "MIT",
                     "name": "ripgrep", "size": 1400000, "subdir": "linux-64",
                     "version": "13.0.0"},
                    {"build": "h8fae777_1", "channel": "conda-forge",
                     "depends": ["libgcc-ng >=12"], "license": "MIT",
                     "name": "ripgrep", "size": 1642417, "subdir": "linux-64",
                     "version": "14.1.1"}
                ], "status": "OK"}}"#,
        )
        .unwrap();
        let packages = parse_search(&json);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "ripgrep");
        assert_eq!(packages[0].version.as_deref(), Some("14.1.1"));
        assert_eq!(packages[0].download_size, Some(1642417));
        assert_eq!(
            packages[0].dependencies,
            Some(vec!["libgcc-ng".to_string()])
        );
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn parses_conda_style_search_map() {
        let json: Value = serde_json::from_str(
            r#"{"hyperfine": [
                {"build": "h0708190_0", "channel": "conda-forge/linux-64",
                 "license": "Apache-2.0 OR MIT", "name": "hyperfine",
                 "size": 500000, "subdir": "linux-64", "version": "1.18.0"},
                {"build": "h3bba108_0", "channel": "conda-forge/linux-64",
                 "license": "Apache-2.0 OR MIT", "name": "hyperfine",
                 "size": 512345, "subdir": "linux-64", "version": "1.19.0"}
            ]}"#,
        )
        .unwrap();
        let packages = parse_search(&json);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "hyperfine");
        assert_eq!(packages[0].version.as_deref(), Some("1.19.0"));
        assert_eq!(packages[0].license.as_deref(), Some("Apache-2.0 OR MIT"));
    }

    #[test]
    fn search_error_body_parses_to_nothing() {
        let json: Value = serde_json::from_str(
            r#"{"error": "PackagesNotFoundError: The following packages are not available",
                "exception_name": "PackagesNotFoundError"}"#,
        )
        .unwrap();
        assert!(parse_search(&json).is_empty());
    }

    #[test]
    fn parses_update_preview_into_outdated() {
        let json: Value = serde_json::from_str(
            r#"{"actions": {
                "FETCH": [],
                "LINK": [
                    {"channel": "conda-forge", "name": "python", "version": "3.13.2"},
                    {"channel": "conda-forge", "name": "tzdata", "version": "2026a"}
                ],
                "PREFIX": "/home/user/micromamba",
                "UNLINK": [
                    {"channel": "conda-forge", "name": "python", "version": "3.13.1"}
                ]
            }, "dry_run": true, "success": true}"#,
        )
        .unwrap();
        let packages = parse_outdated(&json);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "python");
        assert_eq!(packages[0].version.as_deref(), Some("3.13.1"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("3.13.2"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(spec(&PackageRequest::parse("polars@1.9.0")), "polars=1.9.0");
        assert_eq!(spec(&PackageRequest::parse("polars")), "polars");
    }
}
