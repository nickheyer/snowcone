//! Pixi backend for snowcone.
//!
//! Manages `pixi global` tool installs (like npm's `-g`): pixi's project
//! environments belong to project tooling, but its global environments are a
//! per-user, system-wide surface. `pixi global list` has no machine format,
//! so its tree output is line-parsed - both the current `name: version`
//! shape and the pre-0.33 `name version` shape, since the global rework
//! changed the CLI (it also replaced `pixi global upgrade` with `pixi
//! global update`, which this backend uses). `pixi search` prints a
//! key/value block for a single match and a table for several, and exits
//! nonzero on "no match". pixi never prompts, so `assume_yes` is a no-op,
//! and no global subcommand has a dry-run switch, so `--dry-run` requests
//! error out. Pins use conda MatchSpec syntax (`name==version`). Everything
//! is per-user - nothing elevates.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "pixi";
const PROGRAMS: &[&str] = &["pixi"];

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

    /// The globally installed tools, from `pixi global list`.
    async fn installed(&self) -> Result<Vec<PixiPackage>> {
        let output = self
            .query()
            .args(["global", "list"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_global_list(&output.stdout))
    }

    /// `pixi search …`, mapping the "no match" nonzero exit to an empty
    /// result and keeping every other failure an error.
    async fn search_channels(&self, pattern: &str) -> Result<Vec<PixiPackage>> {
        let output = self
            .query()
            .arg("search")
            .arg(pattern)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            let noise = format!("{} {}", output.stdout, output.stderr).to_lowercase();
            if noise.contains("not found") || noise.contains("could not find") {
                return Ok(Vec::new());
            }
            output.require_success()?;
            return Ok(Vec::new());
        }
        Ok(parse_search_output(&output.stdout))
    }
}

/// `name==version` (exact-version MatchSpec) when the request pins one,
/// bare name otherwise.
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
        "Pixi"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Universal
    }

    fn database_id(&self) -> &'static str {
        "pixi"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::UPGRADE
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        let cmd = self
            .cmd()
            .args(["global", "install"])
            .args(packages.iter().map(spec));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("uninstall"));
        }
        let cmd = self
            .cmd()
            .args(["global", "uninstall"])
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
        // The channel view says nothing about the global environments; one
        // list probe fills in the installed state and version.
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
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        if packages.is_empty() {
            return self.run(self.cmd().args(["global", "update"]), ctx).await;
        }
        // `update` only moves to the newest; a pinned move is an install of
        // that exact version.
        if packages.iter().any(|package| package.version.is_some()) {
            let cmd = self
                .cmd()
                .args(["global", "install"])
                .args(packages.iter().map(spec));
            return self.run(cmd, ctx).await;
        }
        let cmd = self
            .cmd()
            .args(["global", "update"])
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }
}

fn boxed(packages: Vec<PixiPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `pixi global list`: tree lines - `├── name: version (exposes: …)` on
/// current pixi, `├─ name version` before the 0.33 global rework - with
/// header and `exposes` lines skipped.
fn parse_global_list(stdout: &str) -> Vec<PixiPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let text = line
                .trim_start_matches(|c: char| {
                    c.is_whitespace() || matches!(c, '├' | '└' | '│' | '─')
                })
                .trim();
            let (name, rest) = text.split_once(':').or_else(|| text.split_once(' '))?;
            let name = name.trim();
            let version = rest.split_whitespace().next()?;
            (!name.is_empty()
                && !name.contains(char::is_whitespace)
                && name != "exposes"
                && version.starts_with(|c: char| c.is_ascii_digit()))
            .then(|| PixiPackage {
                name: name.to_string(),
                version: Some(version.to_string()),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `pixi search` output: a whitespace-aligned table when several packages
/// match, a key/value detail block when exactly one does.
fn parse_search_output(stdout: &str) -> Vec<PixiPackage> {
    if let Some(packages) = parse_search_table(stdout) {
        return packages;
    }
    parse_search_detail(stdout).into_iter().collect()
}

/// The multi-match table: rows after a header line starting
/// `Package Version …` (`None` when no such header exists).
fn parse_search_table(stdout: &str) -> Option<Vec<PixiPackage>> {
    let mut lines = stdout.lines();
    loop {
        let mut tokens = lines.next()?.split_whitespace();
        if matches!(tokens.next(), Some("Package" | "Name")) && tokens.next() == Some("Version") {
            break;
        }
    }
    Some(
        lines
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                Some(PixiPackage {
                    name: parts.next()?.to_string(),
                    version: parts.next().map(str::to_string),
                    origin: parts.next().map(str::to_string),
                    state: InstallState::Available,
                    ..Default::default()
                })
            })
            .collect(),
    )
}

/// The single-match detail block: `Key  Value` lines plus a
/// `Dependencies:` list; multi-word keys (`File Name`) are not carried.
fn parse_search_detail(stdout: &str) -> Option<PixiPackage> {
    let mut package = PixiPackage {
        state: InstallState::Available,
        ..Default::default()
    };
    let mut dependencies: Vec<String> = Vec::new();
    let mut in_dependencies = false;
    for line in stdout.lines() {
        let text = line.trim();
        if text.is_empty() {
            in_dependencies = false;
            continue;
        }
        if in_dependencies {
            if let Some(dep) = text.strip_prefix('-') {
                if let Some(name) = dep.split_whitespace().next() {
                    dependencies.push(name.to_string());
                }
                continue;
            }
            in_dependencies = false;
        }
        if text.starts_with("Dependencies") {
            in_dependencies = true;
            continue;
        }
        let Some((key, value)) = text.split_once(char::is_whitespace) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key {
            "Name" => package.name = value.to_string(),
            "Version" => package.version = Some(value.to_string()),
            "License" => package.license = Some(value.to_string()),
            "Subdir" => package.architecture = Some(value.to_string()),
            "Channel" => package.origin = Some(value.to_string()),
            "Size" => package.download_size = value.parse().ok(),
            _ => {}
        }
    }
    if !dependencies.is_empty() {
        package.dependencies = Some(dependencies);
    }
    (!package.name.is_empty()).then_some(package)
}

/// A package as pixi describes it.
#[derive(Debug, Default)]
pub struct PixiPackage {
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

impl Package for PixiPackage {
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
    fn parses_current_global_list() {
        let stdout = "\
Global environments as specified in '/home/nick/.pixi/manifests/pixi-global.toml'
├── pixi-pack: 0.6.3 (exposes: pixi-pack)
├── python: 3.13.1 (exposes: 2to3, idle3, pydoc, python)
└── ripgrep: 14.1.1 (exposes: rg)
";
        let packages = parse_global_list(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "pixi-pack");
        assert_eq!(packages[0].version.as_deref(), Some("0.6.3"));
        assert_eq!(packages[2].name, "ripgrep");
        assert_eq!(packages[2].version.as_deref(), Some("14.1.1"));
        assert_eq!(packages[2].state, InstallState::Installed);
    }

    #[test]
    fn parses_pre_rework_global_list() {
        let stdout = "\
Global install location: /home/nick/.pixi
├─ python 3.11.3
│  └─ exposes 2to3, idle3, pydoc, python
└─ ruff 0.4.4
   └─ exposes ruff
";
        let packages = parse_global_list(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "python");
        assert_eq!(packages[0].version.as_deref(), Some("3.11.3"));
        assert_eq!(packages[1].name, "ruff");
        assert_eq!(packages[1].version.as_deref(), Some("0.4.4"));
    }

    #[test]
    fn parses_search_detail_block() {
        let stdout = "\
ripgrep-14.1.1-h8fae777_1 (+ 3 builds)
--------------------------------------

Name                ripgrep
Version             14.1.1
Build               h8fae777_1
Size                1642417
License             MIT
Subdir              linux-64
File Name           ripgrep-14.1.1-h8fae777_1.conda
URL                 https://conda.anaconda.org/conda-forge/linux-64/ripgrep-14.1.1-h8fae777_1.conda
Dependencies:
 - libgcc-ng >=12

Other Versions (2):
14.1.0
14.0.3
";
        let package = parse_search_output(stdout);
        assert_eq!(package.len(), 1);
        assert_eq!(package[0].name, "ripgrep");
        assert_eq!(package[0].version.as_deref(), Some("14.1.1"));
        assert_eq!(package[0].license.as_deref(), Some("MIT"));
        assert_eq!(package[0].architecture.as_deref(), Some("linux-64"));
        assert_eq!(package[0].download_size, Some(1642417));
        assert_eq!(package[0].dependencies, Some(vec!["libgcc-ng".to_string()]));
        assert_eq!(package[0].state, InstallState::Available);
    }

    #[test]
    fn parses_search_table() {
        let stdout = "\
Package             Version    Channel
ripgrep             14.1.1     conda-forge
ripgrep-all         0.9.6      conda-forge
";
        let packages = parse_search_output(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "ripgrep");
        assert_eq!(packages[0].version.as_deref(), Some("14.1.1"));
        assert_eq!(packages[0].origin.as_deref(), Some("conda-forge"));
        assert_eq!(packages[1].name, "ripgrep-all");
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(spec(&PackageRequest::parse("ruff@0.4.4")), "ruff==0.4.4");
        assert_eq!(spec(&PackageRequest::parse("ruff")), "ruff");
    }
}
