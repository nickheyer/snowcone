//! RubyGems backend for snowcone.
//!
//! Runs as the user against the active Ruby's gem environment - nothing
//! elevates. RubyGems keeps multiple versions of a gem side by side, so
//! listings report the newest of each, and a pinned upgrade goes through
//! `gem install -v` because `gem update` takes no version. `install` and
//! `update` never prompt and accept `--explain` as a faithful dry run;
//! `gem uninstall` is the one prompting verb, so `assume_yes` maps to its
//! documented non-interactive spelling `-a -x -I` (all versions,
//! executables too, no dependency questions) and it has no dry-run at all.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "gem";
const PROGRAMS: &[&str] = &["gem"];

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
}

/// `gem install` arguments for one request: the name, plus `-v <version>`
/// when pinned. `-v` applies to the whole invocation, hence one gem per run.
fn install_args(request: &PackageRequest) -> Vec<String> {
    match &request.version {
        Some(version) => vec![request.name.clone(), "-v".to_string(), version.clone()],
        None => vec![request.name.clone()],
    }
}

/// Anchor a gem name inside a regex without letting `.`, `+`, … act as
/// metacharacters (`gem search`/`gem info` treat their argument as one).
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
        "RubyGems"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "rubygems"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        for package in packages {
            let mut cmd = self.cmd().arg("install").args(install_args(package));
            if ctx.dry_run {
                cmd = cmd.arg("--explain");
            }
            self.run(cmd, ctx).await?;
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: remove has no dry-run mode")));
        }
        let mut cmd = self.cmd().arg("uninstall");
        if ctx.assume_yes {
            cmd = cmd.args(["-a", "-x", "-I"]);
        }
        cmd = cmd.args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["list", "--local"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_gem_lines(&output.stdout, InstallState::Installed)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // Local first: `gem info` prints the detailed listing for installed
        // gems and an empty banner otherwise.
        let local = self
            .query()
            .arg("info")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if local.success()
            && let Some(package) = parse_details(&local.stdout, InstallState::Installed)
                .into_iter()
                .find(|package| package.name == name)
        {
            return Ok(Box::new(package));
        }
        // Not installed: ask the registry, anchored so the regex matches
        // exactly this gem.
        let pattern = format!("^{}$", regex_escape(name));
        let remote = self
            .query()
            .args(["search", "--remote", "--details"])
            .arg(&pattern)
            .capture(&self.elevator, None)
            .await?;
        if !remote.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        parse_details(&remote.stdout, InstallState::Available)
            .into_iter()
            .find(|package| package.name == name)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["search", "--remote"])
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_gem_lines(&output.stdout, InstallState::Available)))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if packages.is_empty() {
            let mut cmd = self.cmd().arg("update");
            if ctx.dry_run {
                cmd = cmd.arg("--explain");
            }
            return self.run(cmd, ctx).await;
        }
        for package in packages {
            // A pinned target goes through `gem install -v`: RubyGems keeps
            // versions side by side and `gem update` takes no version.
            let mut cmd = match &package.version {
                Some(_) => self.cmd().arg("install").args(install_args(package)),
                None => self.cmd().arg("update").arg(package.name.as_str()),
            };
            if ctx.dry_run {
                cmd = cmd.arg("--explain");
            }
            self.run(cmd, ctx).await?;
        }
        Ok(())
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("outdated")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_outdated(&output.stdout)))
    }
}

fn boxed(packages: Vec<GemPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `name (versions…)` header shared by list/search/details output: the
/// newest version comes first; `default: ` prefixes and platform suffixes
/// are stripped.
fn parse_header(line: &str) -> Option<(String, String)> {
    let (name, rest) = line.split_once(" (")?;
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    let versions = rest.strip_suffix(')')?;
    let newest = versions.split(',').next()?.trim();
    let newest = newest.strip_prefix("default: ").unwrap_or(newest);
    let version = newest.split_whitespace().next()?;
    Some((name.to_string(), version.to_string()))
}

/// `gem list` / `gem search`: one `name (1.2.0, 1.1.0)` line per gem under
/// a `*** … GEMS ***` banner.
fn parse_gem_lines(stdout: &str, state: InstallState) -> Vec<GemPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            if line.starts_with(char::is_whitespace) {
                return None;
            }
            let (name, version) = parse_header(line)?;
            Some(GemPackage {
                name,
                version: Some(version),
                state,
                ..Default::default()
            })
        })
        .collect()
}

/// `gem info` / `gem search --details`: `name (versions)` headers, indented
/// `Key: Value` metadata, then an indented description paragraph after a
/// blank line - description text may itself contain colons, so everything
/// past that blank line is prose.
fn parse_details(stdout: &str, state: InstallState) -> Vec<GemPackage> {
    let mut packages: Vec<GemPackage> = Vec::new();
    let mut in_description = false;
    for line in stdout.lines() {
        if line.trim().is_empty() {
            in_description = !packages.is_empty();
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            if let Some((name, version)) = parse_header(line) {
                packages.push(GemPackage {
                    name,
                    version: Some(version),
                    state,
                    ..Default::default()
                });
                in_description = false;
            }
            continue;
        }
        let Some(package) = packages.last_mut() else {
            continue;
        };
        let text = line.trim();
        if in_description {
            match &mut package.description {
                Some(description) => {
                    description.push(' ');
                    description.push_str(text);
                }
                None => package.description = Some(text.to_string()),
            }
            continue;
        }
        let Some((key, value)) = text.split_once(':') else {
            continue;
        };
        match key.trim() {
            "Homepage" => package.homepage = Some(value.trim().to_string()),
            "License" | "Licenses" => package.license = Some(value.trim().to_string()),
            _ => {}
        }
    }
    packages
}

/// `gem outdated`: `name (current < latest)` lines.
fn parse_outdated(stdout: &str) -> Vec<GemPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once(" (")?;
            if name.is_empty() || name.contains(char::is_whitespace) {
                return None;
            }
            let (current, latest) = rest.strip_suffix(')')?.split_once(" < ")?;
            Some(GemPackage {
                name: name.to_string(),
                version: Some(current.to_string()),
                latest_version: Some(latest.to_string()),
                state: InstallState::Upgradable,
                ..Default::default()
            })
        })
        .collect()
}

/// A package as RubyGems describes it.
#[derive(Debug, Default)]
pub struct GemPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub state: InstallState,
}

impl Package for GemPackage {
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

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_local_list() {
        let stdout = "\n*** LOCAL GEMS ***\n\nbigdecimal (3.1.4)\nbundler (default: 2.4.19)\nnokogiri (1.16.2 x86_64-linux)\nrake (13.0.6, 12.3.3)\n";
        let packages = parse_gem_lines(stdout, InstallState::Installed);
        assert_eq!(packages.len(), 4);
        assert_eq!(packages[0].name, "bigdecimal");
        assert_eq!(packages[0].version.as_deref(), Some("3.1.4"));
        assert_eq!(packages[1].version.as_deref(), Some("2.4.19"));
        assert_eq!(packages[2].version.as_deref(), Some("1.16.2"));
        assert_eq!(packages[3].version.as_deref(), Some("13.0.6"));
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn parses_remote_search() {
        let stdout = "\n*** REMOTE GEMS ***\n\nrails (7.1.2)\nrails-api (0.4.1)\n";
        let packages = parse_gem_lines(stdout, InstallState::Available);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "rails");
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn parses_outdated_lines() {
        let packages = parse_outdated("rack (2.2.8 < 3.0.9)\nrake (13.0.6 < 13.1.0)\n");
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "rack");
        assert_eq!(packages[0].version.as_deref(), Some("2.2.8"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("3.0.9"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn parses_details() {
        let stdout = "\n*** LOCAL GEMS ***\n
rake (13.0.6)
    Authors: Hiroshi SHIBATA, Eric Hodel, Jim Weirich
    Homepage: https://github.com/ruby/rake
    License: MIT
    Installed at: /usr/lib/ruby/gems/3.0.0

    Rake is a Make-like program implemented in Ruby:
    tasks and dependencies are specified in standard Ruby syntax.

rake-compiler (1.2.5)
    Authors: Kouhei Sutou
    Homepage: https://github.com/rake-compiler/rake-compiler
    Licenses: MIT

    Provide a standard and simplified way to build and package native
    extensions.
";
        let packages = parse_details(stdout, InstallState::Installed);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "rake");
        assert_eq!(packages[0].version.as_deref(), Some("13.0.6"));
        assert_eq!(
            packages[0].homepage.as_deref(),
            Some("https://github.com/ruby/rake")
        );
        assert_eq!(packages[0].license.as_deref(), Some("MIT"));
        assert_eq!(
            packages[0].description.as_deref(),
            Some(
                "Rake is a Make-like program implemented in Ruby: \
                 tasks and dependencies are specified in standard Ruby syntax."
            )
        );
        assert_eq!(packages[1].name, "rake-compiler");
        assert_eq!(packages[1].license.as_deref(), Some("MIT"));
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            install_args(&PackageRequest::parse("rails@7.1.2")),
            vec!["rails", "-v", "7.1.2"]
        );
        assert_eq!(install_args(&PackageRequest::parse("rails")), vec!["rails"]);
    }

    #[test]
    fn escapes_regex_metacharacters() {
        assert_eq!(regex_escape("net-http"), "net-http");
        assert_eq!(regex_escape("rexml.rb"), "rexml\\.rb");
    }
}
