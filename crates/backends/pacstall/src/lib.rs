//! Pacstall backend for snowcone.
//!
//! Pacstall is "the AUR for Ubuntu": it builds pacscripts into .deb
//! packages on the shared dpkg database. It re-executes itself under sudo
//! for `-I`/`-R`/`-Up` (and even `-Lu`), and building refuses root, so
//! snowcone never elevates it - the tool prompts on its own. Queries pass
//! `NO_COLOR=1` because pacstall colors output even when piped. `-Up`
//! takes no package arguments, so targeted upgrades go through `-I`, which
//! rebuilds from the latest pacscript. No verb has a dry-run mode.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "pacstall";
const PROGRAMS: &[&str] = &["pacstall"];

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
    /// Mutating invocation, in the user's locale (output is passed
    /// through). Never elevated - pacstall sudos itself.
    fn cmd(&self) -> Cmd {
        Cmd::new(&self.program)
    }

    /// Read invocation with a stable locale and colors off - pacstall only
    /// disables its ANSI colors when `NO_COLOR` is set.
    fn query(&self) -> Cmd {
        Cmd::new(&self.program)
            .env("LC_ALL", "C")
            .env("NO_COLOR", "1")
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

    /// Mutating verb with `-P`/`--disable-prompts` when the caller wants
    /// non-interactive defaults.
    fn mutation(&self, verb: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(verb);
        if ctx.assume_yes {
            cmd = cmd.arg("-P");
        }
        cmd
    }
}

/// Pacscripts always build the version the repository currently carries;
/// there is nothing to pin against.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but pacstall only builds the latest pacscript"
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
        "Pacstall"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "dpkg"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
    }

    /// Pacstall drives sudo itself for mutations - snowcone never elevates
    /// it, but a credential prompt is still coming.
    fn needs_elevation(&self, operation: Operation) -> bool {
        operation.mutates()
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        let cmd = self
            .mutation("-I", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        let cmd = self
            .mutation("-R", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        // Exits 1 with no output when nothing is installed yet.
        let output = self.query().arg("-L").capture(&self.elevator, None).await?;
        if !output.success() && output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(boxed(parse_installed(&output.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // `-Ci` reads the local metadata of an installed package; `-Si`
        // fetches the remote SRCINFO when it is not installed.
        let cached = self
            .query()
            .arg("-Ci")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if cached.success()
            && let Some(package) = parse_cache_info(&cached.stdout)
        {
            return Ok(Box::new(package));
        }
        let remote = self
            .query()
            .arg("-Si")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !remote.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        parse_srcinfo(&remote.stdout)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        // Exits 1 when nothing matches the query.
        let output = self
            .query()
            .arg("-S")
            .arg(query)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() && output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(boxed(parse_search(&output.stdout)))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        if packages.is_empty() {
            return self.run(self.mutation("-Up", ctx), ctx).await;
        }
        // `-Up` refuses package arguments; `-I` rebuilds the named
        // packages from their latest pacscripts.
        let cmd = self
            .mutation("-I", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        // `-Lu` self-elevates like the mutations do, and compares against
        // the remote repositories, so it is slow and may prompt.
        let output = self
            .query()
            .arg("-Lu")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_outdated(&output.stdout)))
    }
}

fn boxed(packages: Vec<PacstallPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `-L` piped: bare installed package names, one per line.
fn parse_installed(stdout: &str) -> Vec<PacstallPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let name = line.split_whitespace().next()?;
            Some(PacstallPackage {
                name: name.to_string(),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `-S`: `name @ repo` lines. Split-package bases show as
/// `name:pkgbase`; the suffix is dropped.
fn parse_search(stdout: &str) -> Vec<PacstallPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let (name, origin) = line.split_once(" @ ")?;
            let name = name.split_whitespace().next_back()?;
            let name = name.strip_suffix(":pkgbase").unwrap_or(name);
            Some(PacstallPackage {
                name: name.to_string(),
                origin: Some(origin.trim().to_string()),
                state: InstallState::Available,
                ..Default::default()
            })
        })
        .collect()
}

/// `-Lu`: tab-indented `name @ repo ( current -> latest )` lines between
/// human chatter; a literal `unknown` stands in for a missing version.
fn parse_outdated(stdout: &str) -> Vec<PacstallPackage> {
    let version = |text: &str| Some(text.to_string()).filter(|text| text != "unknown");
    stdout
        .lines()
        .filter_map(|line| {
            let (front, versions) = line.split_once(" ( ")?;
            let (name, origin) = front.trim().split_once(" @ ")?;
            let (current, latest) = versions.trim_end().trim_end_matches(')').split_once(" -> ")?;
            Some(PacstallPackage {
                name: name.trim().to_string(),
                version: version(current.trim()),
                latest_version: version(latest.trim()),
                origin: Some(origin.trim().to_string()),
                state: InstallState::Upgradable,
                ..Default::default()
            })
        })
        .collect()
}

/// `-Ci`: `key: value` lines from the installed-package metadata;
/// dependency values are space-separated.
fn parse_cache_info(stdout: &str) -> Option<PacstallPackage> {
    let mut package = PacstallPackage {
        state: InstallState::Installed,
        ..Default::default()
    };
    for line in stdout.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if value.is_empty() {
            continue;
        }
        match key {
            "name" => package.name = value.to_string(),
            "version" => package.version = Some(value.to_string()),
            "description" => package.description = Some(value.to_string()),
            "homepage" => package.homepage = Some(value.to_string()),
            "license" => package.license = Some(value.to_string()),
            "remote repo" => package.origin = Some(value.to_string()),
            "dependencies" => {
                package.dependencies =
                    Some(value.split_whitespace().map(str::to_string).collect());
            }
            _ => {}
        }
    }
    (!package.name.is_empty()).then_some(package)
}

/// `-Si`: a `--- repo ---` banner followed by a makepkg-style SRCINFO
/// block - `key = value` lines, `depends`/`arch` repeatable. Only the
/// first block is read.
fn parse_srcinfo(stdout: &str) -> Option<PacstallPackage> {
    let mut package = PacstallPackage {
        state: InstallState::Available,
        ..Default::default()
    };
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(banner) = trimmed.strip_prefix("---") {
            let repo = banner.trim_end_matches('-').trim();
            if !repo.is_empty() {
                if package.origin.is_some() {
                    break;
                }
                package.origin = Some(repo.to_string());
            }
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if value.is_empty() {
            continue;
        }
        match key {
            "pkgbase" | "pkgname" if package.name.is_empty() => {
                package.name = value.to_string();
            }
            "pkgver" => package.version = Some(value.to_string()),
            "pkgdesc" => package.description = Some(value.to_string()),
            "url" | "homepage" => package.homepage = Some(value.to_string()),
            "license" => package.license = Some(value.to_string()),
            "arch" if package.architecture.is_none() => {
                package.architecture = Some(value.to_string());
            }
            "depends" => {
                package
                    .dependencies
                    .get_or_insert_with(Vec::new)
                    .push(value.to_string());
            }
            _ => {}
        }
    }
    (!package.name.is_empty()).then_some(package)
}

/// A package as pacstall describes it.
#[derive(Debug, Default)]
pub struct PacstallPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub architecture: Option<String>,
    pub origin: Option<String>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for PacstallPackage {
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

    fn architecture(&self) -> Option<&str> {
        self.architecture.as_deref()
    }

    fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
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
    fn parses_installed_names() {
        let packages = parse_installed("neofetch\nbottom-bin\n");
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "bottom-bin");
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn parses_search_lines() {
        let stdout = "\
neofetch @ pacstall
neofetch-git @ pacstall
mesa:pkgbase @ pacstall
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "neofetch");
        assert_eq!(packages[0].origin.as_deref(), Some("pacstall"));
        assert_eq!(packages[2].name, "mesa");
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn parses_upgrade_listing() {
        let stdout = "\
[+] INFO: Checking for updates
[+] INFO: Packages can be upgraded
Upgradable: 2
\tneofetch @ pacstall ( 7.1.0 -> 7.2.0 )
\tbottom-bin @ pacstall ( unknown -> 0.10.2 )

";
        let packages = parse_outdated(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "neofetch");
        assert_eq!(packages[0].version.as_deref(), Some("7.1.0"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("7.2.0"));
        assert_eq!(packages[0].origin.as_deref(), Some("pacstall"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
        assert_eq!(packages[1].version, None);
        assert_eq!(packages[1].latest_version.as_deref(), Some("0.10.2"));
    }

    #[test]
    fn parses_cache_info() {
        let stdout = "\
name: neofetch
version: 7.1.0-3
size: 856 KB
description: A command-line system information tool
date installed: Tue Jul 29 14:02:11 2025
homepage: https://github.com/dylanaraps/neofetch
license: MIT
remote repo: https://github.com/pacstall/pacstall-programs
maintainer: Pacstall Team <team@pacstall.dev>
dependencies: bash caca-utils chafa
install type: explicitly installed
";
        let package = parse_cache_info(stdout).unwrap();
        assert_eq!(package.name, "neofetch");
        assert_eq!(package.version.as_deref(), Some("7.1.0-3"));
        assert_eq!(
            package.homepage.as_deref(),
            Some("https://github.com/dylanaraps/neofetch")
        );
        assert_eq!(package.license.as_deref(), Some("MIT"));
        assert_eq!(
            package.origin.as_deref(),
            Some("https://github.com/pacstall/pacstall-programs")
        );
        assert_eq!(
            package.dependencies,
            Some(vec![
                "bash".to_string(),
                "caca-utils".to_string(),
                "chafa".to_string()
            ])
        );
        assert_eq!(package.state, InstallState::Installed);
    }

    #[test]
    fn parses_srcinfo_first_block() {
        let stdout = "\
--- pacstall ---
pkgbase = abdownloadmanager
\tpkgver = 1.10.1
\tpkgdesc = Download manager
\turl = https://abdownloadmanager.com
\tarch = amd64
\tarch = arm64
\tlicense = Apache-2.0
\tdepends = libgtk-3-0
\tdepends = libglib2.0-0

pkgname = abdownloadmanager
--- other-repo ---
pkgbase = abdownloadmanager
\tpkgver = 0.9.0
";
        let package = parse_srcinfo(stdout).unwrap();
        assert_eq!(package.name, "abdownloadmanager");
        assert_eq!(package.version.as_deref(), Some("1.10.1"));
        assert_eq!(package.description.as_deref(), Some("Download manager"));
        assert_eq!(package.architecture.as_deref(), Some("amd64"));
        assert_eq!(package.license.as_deref(), Some("Apache-2.0"));
        assert_eq!(package.origin.as_deref(), Some("pacstall"));
        assert_eq!(
            package.dependencies,
            Some(vec!["libgtk-3-0".to_string(), "libglib2.0-0".to_string()])
        );
        assert_eq!(package.state, InstallState::Available);
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("neofetch@7.1.0")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("neofetch")]).is_ok());
    }
}
