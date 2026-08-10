//! dub backend for snowcone.
//!
//! Manages D's per-user package cache: `dub fetch` downloads a release
//! into it, `dub remove` deletes from it, and several versions of a
//! package coexist there. dub's own `upgrade` verb is project-scoped, so
//! upgrade here re-fetches the newest release into the cache instead.
//! There is no info verb either - info combines a registry search with
//! the cache listing. dub never prompts and has no dry-run mode, and
//! fetch/remove take one package per invocation, so batch operations
//! loop.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "dub";
const PROGRAMS: &[&str] = &["dub"];

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

    /// Everything in the package cache, from `dub list`.
    async fn cached(&self) -> Result<Vec<DubPackage>> {
        let output = self
            .query()
            .arg("list")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_list(&output.stdout))
    }

    /// Registry matches for `query`. dub exits non-zero when nothing
    /// matches, so the parsed stdout is authoritative, not the exit code.
    async fn search_registry(&self, query: &str) -> Result<Vec<DubPackage>> {
        let output = self
            .query()
            .arg("search")
            .arg(query)
            .capture(&self.elevator, None)
            .await?;
        Ok(parse_search(&output.stdout))
    }
}

/// `name@version` when the request pins one, bare name otherwise.
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
        "dub"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "dub"
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
        for package in packages {
            self.run(self.cmd().arg("fetch").arg(spec(package)), ctx)
                .await?;
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        for package in packages {
            self.run(self.cmd().arg("remove").arg(&package.name), ctx)
                .await?;
        }
        Ok(())
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.cached().await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // The registry may be unreachable; the cache alone still answers.
        let remote = self
            .search_registry(name)
            .await
            .ok()
            .and_then(|matches| matches.into_iter().find(|package| package.name == name));
        let installed = self
            .cached()
            .await?
            .into_iter()
            .rev() // the last listed version of a package wins
            .find(|package| package.name == name);
        let package = match (remote, installed) {
            (Some(mut package), Some(installed)) => {
                package.state = InstallState::Installed;
                if installed.version.is_some() && installed.version != package.version {
                    package.latest_version = package.version.take();
                    package.version = installed.version;
                }
                package
            }
            (Some(package), None) => package,
            (None, Some(installed)) => installed,
            (None, None) => return Err(Error::NotFound(name.to_string())),
        };
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.search_registry(query).await?))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        // `dub upgrade` only touches a project; fetching again brings the
        // newest release into the cache.
        if packages.is_empty() {
            let mut names: Vec<String> = self
                .cached()
                .await?
                .into_iter()
                .map(|package| package.name)
                .collect();
            names.sort();
            names.dedup();
            for name in names {
                self.run(self.cmd().arg("fetch").arg(&name), ctx).await?;
            }
            return Ok(());
        }
        for package in packages {
            self.run(self.cmd().arg("fetch").arg(spec(package)), ctx)
                .await?;
        }
        Ok(())
    }
}

fn boxed(packages: Vec<DubPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `dub list`: a banner line, then one indented `name version: path` entry
/// per cached package version.
fn parse_list(stdout: &str) -> Vec<DubPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            if !line.starts_with(char::is_whitespace) {
                return None;
            }
            let (package, _path) = line.split_once(':')?;
            let mut parts = package.split_whitespace();
            let name = parts.next()?;
            let version = parts.next()?;
            Some(DubPackage {
                name: name.to_string(),
                version: Some(version.to_string()),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `dub search`: `name (version): description` entries below a banner
/// line; tolerant of indentation and of `-` or nothing after the closing
/// paren.
fn parse_search(stdout: &str) -> Vec<DubPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let (name, rest) = line.trim().split_once(" (")?;
            if name.is_empty() || name.contains(char::is_whitespace) {
                return None;
            }
            let (version, rest) = rest.split_once(')')?;
            if !version.starts_with(|c: char| c.is_ascii_digit() || c == '~') {
                return None;
            }
            let description = rest.trim().trim_start_matches([':', '-']).trim_start();
            Some(DubPackage {
                name: name.to_string(),
                version: Some(version.to_string()),
                description: (!description.is_empty()).then(|| description.to_string()),
                state: InstallState::Available,
                ..Default::default()
            })
        })
        .collect()
}

/// A package as dub describes it.
#[derive(Debug, Default)]
pub struct DubPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for DubPackage {
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

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cache_list() {
        let stdout = "\
Packages present in the system and known to dub:
  dub 1.38.1: /home/nick/.dub/packages/dub/1.38.1/dub/
  vibe-d 0.9.5: /home/nick/.dub/packages/vibe-d/0.9.5/vibe-d/
  vibe-d 0.10.1: /home/nick/.dub/packages/vibe-d/0.10.1/vibe-d/
";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "dub");
        assert_eq!(packages[0].version.as_deref(), Some("1.38.1"));
        assert_eq!(packages[2].version.as_deref(), Some("0.10.1"));
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn parses_search_entries() {
        let stdout = "\
==== Search results for \"vibe\" ====
  vibe-d (0.10.1): Event driven web and concurrency framework
  vibe-core (2.8.0) The I/O and concurrency core of vibe.d
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "vibe-d");
        assert_eq!(packages[0].version.as_deref(), Some("0.10.1"));
        assert_eq!(
            packages[0].description.as_deref(),
            Some("Event driven web and concurrency framework")
        );
        assert_eq!(packages[1].name, "vibe-core");
        assert_eq!(
            packages[1].description.as_deref(),
            Some("The I/O and concurrency core of vibe.d")
        );
        assert_eq!(packages[1].state, InstallState::Available);
    }

    #[test]
    fn search_skips_banner_lines() {
        let packages = parse_search("==== dub.org: 2 results for \"vibe\" ====\n");
        assert!(packages.is_empty());
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("vibe-d@0.10.1")),
            "vibe-d@0.10.1"
        );
        assert_eq!(spec(&PackageRequest::parse("vibe-d")), "vibe-d");
    }
}
