//! Nimble backend for snowcone.
//!
//! Drives Nim's nimble against the per-user package store (~/.nimble).
//! `refresh` is a real verb that re-downloads the package list. nimble
//! asks questions mid-run, so `-y` goes on mutations when `assume_yes` is
//! set; without it the prompts pass through interactively. Several
//! versions of a package install side by side, there is no upgrade verb
//! (installing again picks up the newest release) and no outdated listing
//! at all - that stub capability is dropped. No dry-run mode anywhere;
//! batch operations loop one package per invocation.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "nimble";
const PROGRAMS: &[&str] = &["nimble"];

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

    /// Shared shape for mutating verbs: the verb plus `-y` under
    /// `assume_yes`.
    fn mutation(&self, verb: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(verb);
        if ctx.assume_yes {
            cmd = cmd.arg("-y");
        }
        cmd
    }

    /// Installed packages, from `nimble list -i`.
    async fn installed(&self) -> Result<Vec<NimblePackage>> {
        let output = self
            .query()
            .args(["list", "-i"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_installed(&output.stdout))
    }

    /// Registry matches for `query`. nimble exits non-zero when nothing
    /// matches, so the parsed stdout is authoritative, not the exit code.
    async fn search_registry(&self, query: &str) -> Result<Vec<NimblePackage>> {
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
        "Nimble"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "nimble"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::REFRESH
            | Capabilities::UPGRADE
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        for package in packages {
            self.run(self.mutation("install", ctx).arg(spec(package)), ctx)
                .await?;
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        for package in packages {
            self.run(self.mutation("uninstall", ctx).arg(&package.name), ctx)
                .await?;
        }
        Ok(())
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.installed().await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // The registry entry carries the metadata; the installed list
        // fills in state and version. Nim package names compare
        // case-insensitively.
        let remote = self
            .search_registry(name)
            .await?
            .into_iter()
            .find(|package| package.name.eq_ignore_ascii_case(name));
        let installed = self
            .installed()
            .await?
            .into_iter()
            .find(|package| package.name.eq_ignore_ascii_case(name));
        let package = match (remote, installed) {
            (Some(mut package), Some(installed)) => {
                package.state = InstallState::Installed;
                package.version = installed.version;
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

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("refresh"));
        }
        self.run(self.cmd().arg("refresh"), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        // No upgrade verb: installing again picks up the newest release.
        if packages.is_empty() {
            for package in self.installed().await? {
                self.run(self.mutation("install", ctx).arg(&package.name), ctx)
                    .await?;
            }
            return Ok(());
        }
        for package in packages {
            self.run(self.mutation("install", ctx).arg(spec(package)), ctx)
                .await?;
        }
        Ok(())
    }
}

fn boxed(packages: Vec<NimblePackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `list -i`: `name  [versions]` lines; side-by-side installs appear
/// comma-separated inside the brackets and the last (newest) one wins.
fn parse_installed(stdout: &str) -> Vec<NimblePackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let name = line.split_whitespace().next()?;
            let version = line
                .split_once('[')
                .map(|(_, versions)| versions.trim_end().trim_end_matches(']'))
                .and_then(|versions| versions.split(',').next_back())
                .map(|version| version.trim().to_string())
                .filter(|version| !version.is_empty());
            Some(NimblePackage {
                name: name.to_string(),
                version,
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `search`: a `name:` header per match with indented `key: value` fields
/// (url, tags, description, license, website) below it; `url` carries a
/// trailing ` (git)`-style fetch-method note.
fn parse_search(stdout: &str) -> Vec<NimblePackage> {
    let mut packages: Vec<NimblePackage> = Vec::new();
    for line in stdout.lines() {
        if !line.starts_with(char::is_whitespace) {
            if let Some(name) = line.trim().strip_suffix(':')
                && !name.is_empty()
                && !name.contains(char::is_whitespace)
            {
                packages.push(NimblePackage {
                    name: name.to_string(),
                    state: InstallState::Available,
                    ..Default::default()
                });
            }
            continue;
        }
        let Some(last) = packages.last_mut() else {
            continue;
        };
        let Some((key, value)) = line.trim().split_once(':') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "description" => last.description = Some(value.to_string()),
            "license" => last.license = Some(value.to_string()),
            "website" => last.homepage = Some(value.to_string()),
            // Only fills in when no website field follows.
            "url" if last.homepage.is_none() => {
                let url = value.split_once(" (").map_or(value, |(url, _)| url);
                last.homepage = Some(url.trim().to_string());
            }
            _ => {}
        }
    }
    packages
}

/// A package as nimble describes it.
#[derive(Debug, Default)]
pub struct NimblePackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub state: InstallState,
}

impl Package for NimblePackage {
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
    fn parses_installed_list() {
        let stdout = "\
cligen  [1.7.5]
jester  [0.5.0, 0.6.0]
";
        let packages = parse_installed(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "cligen");
        assert_eq!(packages[0].version.as_deref(), Some("1.7.5"));
        assert_eq!(packages[1].version.as_deref(), Some("0.6.0"));
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn parses_search_entries() {
        let stdout = "\
jester:
  url:         https://github.com/dom96/jester (git)
  tags:        web, http, framework, dsl
  description: A sinatra-like web framework for Nim.
  license:     MIT
  website:     https://jester.example.org

httpbeast:
  url:         https://github.com/dom96/httpbeast (git)
  tags:        web, http, server
  description: A super-fast epoll-backed HTTP server.
  license:     MIT
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "jester");
        assert_eq!(
            packages[0].description.as_deref(),
            Some("A sinatra-like web framework for Nim.")
        );
        assert_eq!(packages[0].license.as_deref(), Some("MIT"));
        assert_eq!(
            packages[0].homepage.as_deref(),
            Some("https://jester.example.org")
        );
        assert_eq!(packages[0].state, InstallState::Available);
        // No website field: the url minus its fetch-method note fills in.
        assert_eq!(
            packages[1].homepage.as_deref(),
            Some("https://github.com/dom96/httpbeast")
        );
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(spec(&PackageRequest::parse("jester@0.6.0")), "jester@0.6.0");
        assert_eq!(spec(&PackageRequest::parse("jester")), "jester");
    }
}
