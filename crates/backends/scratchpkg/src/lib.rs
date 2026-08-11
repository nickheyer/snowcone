//! scratchpkg backend for snowcone.
//!
//! Venom Linux's `scratch` builds ports from source. Its verbs and output
//! come straight from the scratch script itself: `installed` lists
//! `name version-release` lines, `info` prints `Key: Value` fields (with
//! `Installed: -` for a port that is not installed), `search` prints
//! `[*] (repo) name version-release: description` rows (`[ ]` when not
//! installed), and `sync` runs portsync, the ports-tree refresh. The
//! parsers still strip ANSI colour codes because the script colours its
//! output. scratch checks for root (needroot) and exits rather than
//! escalating itself, so snowcone elevates mutations - portsync included,
//! since it refuses non-root too. No dry-run flag and no yes-flag exist,
//! so dry runs error and prompts stay interactive.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "scratchpkg";
const PROGRAMS: &[&str] = &["scratch"];

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

    async fn installed_ports(&self) -> Result<Vec<ScratchpkgPackage>> {
        let output = self
            .query()
            .arg("installed")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_installed(&output.stdout))
    }

    async fn search_ports(&self, pattern: &str) -> Result<Vec<ScratchpkgPackage>> {
        let output = self
            .query()
            .arg("search")
            .arg(pattern)
            .capture(&self.elevator, None)
            .await?;
        // Tolerate a non-zero "nothing matched" exit as long as nothing
        // was printed either.
        if !output.success() && output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(parse_search(&output.stdout))
    }
}

/// scratch builds whatever version the ports tree carries: nothing to pin.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but scratch builds whatever version the ports tree carries"
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
        "scratchpkg"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "scratchpkg"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::SEARCH | Capabilities::REFRESH | Capabilities::UPGRADE
    }

    fn needs_elevation(&self, operation: Operation) -> bool {
        matches!(
            operation,
            Operation::Install | Operation::Remove | Operation::Upgrade | Operation::Refresh
        )
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        let cmd = self
            .cmd()
            .arg("install")
            .elevated(true)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        let cmd = self
            .cmd()
            .arg("remove")
            .elevated(true)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.installed_ports().await?))
    }

    /// `scratch info` answers directly for any port in the tree; an
    /// unknown port makes it print its not-found message and exit
    /// non-zero.
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

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.search_ports(query).await?))
    }

    /// `scratch sync` runs portsync, the ports-tree refresh; portsync
    /// refuses non-root, so this elevates like every other mutation.
    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("refresh"));
        }
        self.run(self.cmd().arg("sync").elevated(true), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        let cmd = if packages.is_empty() {
            self.cmd().arg("sysup").elevated(true)
        } else {
            self.cmd()
                .arg("upgrade")
                .elevated(true)
                .args(packages.iter().map(|package| package.name.as_str()))
        };
        self.run(cmd, ctx).await
    }
}

fn boxed(packages: Vec<ScratchpkgPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// Drop ANSI CSI sequences (`ESC [ … <letter>`), which scratchpkg's shell
/// scripts may emit.
fn strip_ansi(line: &str) -> String {
    let mut stripped = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            stripped.push(c);
            continue;
        }
        if chars.next() == Some('[') {
            for code in chars.by_ref() {
                if code.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    stripped
}

/// `scratch installed`: assumed `name version` lines (the format is
/// undocumented); anything not shaped like that is skipped.
fn parse_installed(stdout: &str) -> Vec<ScratchpkgPackage> {
    stdout
        .lines()
        .filter_map(|raw| {
            let line = strip_ansi(raw);
            let tokens: Vec<&str> = line.split_whitespace().collect();
            let (name, version) = match *tokens.as_slice() {
                [name] => (name, None),
                [name, version] if version.chars().any(|c| c.is_ascii_digit()) => {
                    (name, Some(version))
                }
                _ => return None,
            };
            Some(ScratchpkgPackage {
                name: name.to_string(),
                version: version.map(str::to_string),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `scratch search`: `[*] (repo) name version-release: description` rows,
/// `[ ]` when the port is not installed (scratch_search's printf). The
/// `(repo)` token anchors each entry; the version token carries the `:`
/// separator, and everything after it is the description.
fn parse_search(stdout: &str) -> Vec<ScratchpkgPackage> {
    stdout
        .lines()
        .filter_map(|raw| {
            let line = strip_ansi(raw);
            let tokens: Vec<&str> = line.split_whitespace().collect();
            let anchor = tokens.iter().position(|token| {
                token.len() > 2 && token.starts_with('(') && token.ends_with(')')
            })?;
            let origin = &tokens[anchor][1..tokens[anchor].len() - 1];
            let name = tokens.get(anchor + 1)?;
            if name.is_empty() {
                return None;
            }
            let version = tokens
                .get(anchor + 2)
                .map(|token| token.trim_end_matches(':'))
                .filter(|candidate| {
                    !candidate.is_empty() && candidate.chars().any(|c| c.is_ascii_digit())
                })?;
            let description = tokens
                .get(anchor + 3..)
                .map(|tail| tail.join(" "))
                .filter(|tail| !tail.is_empty());
            Some(ScratchpkgPackage {
                name: (*name).to_string(),
                version: Some(version.to_string()),
                description,
                origin: Some(origin.to_string()),
                state: if line.trim_start().starts_with("[*]") {
                    InstallState::Installed
                } else {
                    InstallState::Available
                },
            })
        })
        .collect()
}

/// `scratch info`: `Key: Value` fields straight from the script -
/// `Version`/`Release` join into scratchpkg's `version-release` identity,
/// `Path` names the port directory (its parent is the repository), and
/// `Installed` carries the installed `version-release` or `-` when the
/// port is not installed.
fn parse_info(stdout: &str) -> Option<ScratchpkgPackage> {
    let mut package = ScratchpkgPackage {
        state: InstallState::Available,
        ..Default::default()
    };
    let (mut version, mut release, mut installed) = (None, None, None);
    for line in stdout.lines() {
        let Some((key, value)) = strip_ansi(line)
            .split_once(':')
            .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        match key.as_str() {
            "Name" => package.name = value,
            "Path" => {
                package.origin = Path::new(&value)
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|repo| repo.to_str())
                    .map(str::to_string);
            }
            "Version" => version = Some(value),
            "Release" => release = Some(value),
            "Description" => package.description = Some(value),
            // `-` is the script printing empty version-release fields.
            "Installed" if value != "-" => installed = Some(value),
            _ => {}
        }
    }
    package.version = match (version, release) {
        (Some(version), Some(release)) => Some(format!("{version}-{release}")),
        (version, None) => version,
        (None, _) => None,
    };
    if let Some(installed) = installed {
        package.state = InstallState::Installed;
        package.version = Some(installed);
    }
    (!package.name.is_empty()).then_some(package)
}

/// A package as scratch describes it.
#[derive(Debug, Default)]
pub struct ScratchpkgPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub origin: Option<String>,
    pub state: InstallState,
}

impl Package for ScratchpkgPackage {
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
    fn strips_ansi_sequences() {
        assert_eq!(
            strip_ansi("\u{1b}[1;32mzlib\u{1b}[0m 1.3.1-1"),
            "zlib 1.3.1-1"
        );
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn parses_installed_lines() {
        let packages = parse_installed("zlib 1.3.1-1\n\u{1b}[36mncurses\u{1b}[0m 6.4-2\n");
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "ncurses");
        assert_eq!(packages[1].version.as_deref(), Some("6.4-2"));
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn installed_prose_is_skipped() {
        assert!(parse_installed("no packages found here\n").is_empty());
    }

    #[test]
    fn parses_search_entries() {
        let stdout = "\
[ ] (core) zlib 1.3.1-1: compression library
[ ] (main) zlib-ng 2.1.6-1: zlib for next generation systems
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "zlib");
        assert_eq!(packages[0].origin.as_deref(), Some("core"));
        assert_eq!(packages[0].version.as_deref(), Some("1.3.1-1"));
        assert_eq!(
            packages[0].description.as_deref(),
            Some("compression library")
        );
        assert_eq!(packages[0].state, InstallState::Available);
        assert_eq!(
            packages[1].description.as_deref(),
            Some("zlib for next generation systems")
        );
    }

    #[test]
    fn search_marks_installed_entries_and_strips_colour() {
        // The `*` marker and repo name are coloured by the script.
        let stdout = "[\u{1b}[0;32m*\u{1b}[0m] \u{1b}[0;35m(core)\u{1b}[0m zlib \u{1b}[0;36m1.3.1-1\u{1b}[0m: compression library\n";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "zlib");
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(
            packages[0].description.as_deref(),
            Some("compression library")
        );
    }

    #[test]
    fn parses_info_fields() {
        let stdout = "\
Name:         zlib
Path:         /usr/ports/core/zlib
Version:      1.3.1
Release:      1
Description:  compression library
Maintainer:   emmett1, emmett1 dot 2miligrams at gmail dot com
Homepage:     https://zlib.net
Dependencies:
Installed:    1.3.1-1
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.name, "zlib");
        assert_eq!(package.origin.as_deref(), Some("core"));
        assert_eq!(package.version.as_deref(), Some("1.3.1-1"));
        assert_eq!(package.description.as_deref(), Some("compression library"));
        assert_eq!(package.state, InstallState::Installed);
    }

    #[test]
    fn info_dash_means_not_installed() {
        let stdout = "\
Name:         zlib-ng
Path:         /usr/ports/main/zlib-ng
Version:      2.1.6
Release:      1
Description:  zlib for next generation systems
Installed:    -
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.version.as_deref(), Some("2.1.6-1"));
        assert_eq!(package.state, InstallState::Available);
        assert!(parse_info("port not found\n").is_none());
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("zlib@1.3.1")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("zlib")]).is_ok());
    }
}
