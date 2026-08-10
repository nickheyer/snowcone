//! Bundler backend for snowcone.
//!
//! Project-scoped: every operation acts on the Gemfile bundler resolves
//! from snowcone's working directory (walking upward, exactly as `bundle`
//! itself does), so this backend manages the current project's bundle, not
//! a global install set. Install and remove edit the Gemfile via
//! `bundle add`/`bundle remove`; the Gemfile owns version constraints, so
//! version pins are rejected. `bundle outdated` exits non-zero whenever
//! updates exist - a result, not a failure. Bundler never prompts, so
//! `assume_yes` has nothing to do, and no mutation has a dry-run flag.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "bundler";
const PROGRAMS: &[&str] = &["bundle"];

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

/// The Gemfile owns version constraints; a CLI pin has no honest mapping.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but the bundle's Gemfile governs versions"
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
        "Bundler"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "rubygems"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::UPGRADE | Capabilities::LIST_OUTDATED
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: install has no dry-run mode")));
        }
        // One gem per `bundle add` run: multi-gem invocations only exist on
        // recent bundlers.
        for package in packages {
            self.run(self.cmd().arg("add").arg(package.name.as_str()), ctx)
                .await?;
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: remove has no dry-run mode")));
        }
        let cmd = self
            .cmd()
            .arg("remove")
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("list")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_list(&output.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query()
            .arg("info")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        parse_info(&output.stdout)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: upgrade has no dry-run mode")));
        }
        let cmd = if packages.is_empty() {
            // Bundler 2 refuses a bare `bundle update`; `--all` is the verb.
            self.cmd().args(["update", "--all"])
        } else {
            self.cmd()
                .arg("update")
                .args(packages.iter().map(|package| package.name.as_str()))
        };
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("outdated")
            .capture(&self.elevator, None)
            .await?;
        let packages = parse_outdated(&output.stdout);
        // Non-zero exit with parsed rows means "updates exist"; non-zero
        // with nothing parsed is a real failure (no Gemfile, no lockfile…).
        if !output.success() && packages.is_empty() {
            output.require_success()?;
        }
        Ok(boxed(packages))
    }
}

fn boxed(packages: Vec<BundlerPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `* name (version)` bullet shared by `bundle list` and `bundle info`.
fn parse_entry(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("* ")?;
    let (name, version) = rest.split_once(" (")?;
    let version = version.strip_suffix(')')?.split_whitespace().next()?;
    Some((name.to_string(), version.to_string()))
}

/// `bundle list`: one indented `* name (1.2.3)` bullet per gem in the
/// bundle, between a banner line and a `bundle info` hint.
fn parse_list(stdout: &str) -> Vec<BundlerPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let (name, version) = parse_entry(line.trim())?;
            Some(BundlerPackage {
                name,
                version: Some(version),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `bundle info`: a `* name (version)` header with indented `Key: Value`
/// details; only Summary and Homepage map to package metadata.
fn parse_info(stdout: &str) -> Option<BundlerPackage> {
    let mut package: Option<BundlerPackage> = None;
    for line in stdout.lines() {
        let trimmed = line.trim();
        match package.as_mut() {
            None => {
                if let Some((name, version)) = parse_entry(trimmed) {
                    package = Some(BundlerPackage {
                        name,
                        version: Some(version),
                        state: InstallState::Installed,
                        ..Default::default()
                    });
                }
            }
            Some(package) => {
                let Some((key, value)) = trimmed.split_once(':') else {
                    continue;
                };
                match key.trim() {
                    "Summary" => package.description = Some(value.trim().to_string()),
                    "Homepage" => package.homepage = Some(value.trim().to_string()),
                    _ => {}
                }
            }
        }
    }
    package
}

/// `bundle outdated`: either the modern `Gem Current Latest …` table or the
/// legacy `* name (newest X, installed Y)` bullets, depending on bundler
/// version.
fn parse_outdated(stdout: &str) -> Vec<BundlerPackage> {
    let mut packages = Vec::new();
    let mut in_table = false;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(bullet) = trimmed.strip_prefix("* ") {
            let Some((name, rest)) = bullet.split_once(" (newest ") else {
                continue;
            };
            let Some((latest, rest)) = rest.split_once(", installed ") else {
                continue;
            };
            let current = &rest[..rest.find([',', ')']).unwrap_or(rest.len())];
            packages.push(BundlerPackage {
                name: name.to_string(),
                version: Some(current.to_string()),
                latest_version: Some(latest.to_string()),
                state: InstallState::Upgradable,
                ..Default::default()
            });
            continue;
        }
        if !in_table {
            let mut heads = trimmed.split_whitespace();
            in_table = heads.next() == Some("Gem") && heads.next() == Some("Current");
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let (Some(name), Some(current), Some(latest)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        packages.push(BundlerPackage {
            name: name.to_string(),
            version: Some(current.to_string()),
            latest_version: Some(latest.to_string()),
            state: InstallState::Upgradable,
            ..Default::default()
        });
    }
    packages
}

/// A package as bundler describes it.
#[derive(Debug, Default)]
pub struct BundlerPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub state: InstallState,
}

impl Package for BundlerPackage {
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

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bundle_list() {
        let stdout = "\
Gems included by the bundle:
  * CFPropertyList (2.3.6)
  * addressable (2.8.5)
  * rake (13.0.6)
Use `bundle info` to print more detailed information about a gem
";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "CFPropertyList");
        assert_eq!(packages[0].version.as_deref(), Some("2.3.6"));
        assert_eq!(packages[2].name, "rake");
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn parses_bundle_info() {
        let stdout = "\
  * rake (13.0.6)
\tSummary: Rake is a Make-like program implemented in Ruby
\tHomepage: https://github.com/ruby/rake
\tSource Code: https://github.com/ruby/rake
\tPath: /var/lib/gems/3.1.0/gems/rake-13.0.6
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.name, "rake");
        assert_eq!(package.version.as_deref(), Some("13.0.6"));
        assert_eq!(
            package.description.as_deref(),
            Some("Rake is a Make-like program implemented in Ruby")
        );
        assert_eq!(
            package.homepage.as_deref(),
            Some("https://github.com/ruby/rake")
        );
        assert_eq!(package.state, InstallState::Installed);
    }

    #[test]
    fn parses_outdated_table() {
        let stdout = "\
Fetching gem metadata from https://rubygems.org/.........

Gem   Current  Latest  Requested  Groups
rack  2.2.8    3.0.9   >= 0       default
rake  13.0.6   13.1.0
";
        let packages = parse_outdated(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "rack");
        assert_eq!(packages[0].version.as_deref(), Some("2.2.8"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("3.0.9"));
        assert_eq!(packages[1].name, "rake");
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn parses_outdated_bullets() {
        let stdout = "\
Outdated gems included in the bundle:
  * rack (newest 3.0.9, installed 2.2.8, requested ~> 2.2) in groups \"default\"
  * rake (newest 13.1.0, installed 13.0.6)
";
        let packages = parse_outdated(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "rack");
        assert_eq!(packages[0].version.as_deref(), Some("2.2.8"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("3.0.9"));
        assert_eq!(packages[1].version.as_deref(), Some("13.0.6"));
        assert_eq!(packages[1].latest_version.as_deref(), Some("13.1.0"));
    }

    #[test]
    fn up_to_date_bundle_parses_to_nothing() {
        assert!(parse_outdated("Bundle up to date!\n").is_empty());
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("rake@13.0.6")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("rake")]).is_ok());
    }
}
