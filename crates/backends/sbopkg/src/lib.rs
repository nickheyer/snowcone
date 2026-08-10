//! sbopkg backend for snowcone.
//!
//! sbopkg builds SlackBuilds.org packages from source and installs them.
//! It has no remove verb at all - removal honestly delegates to pkgtools'
//! `removepkg` (whose `-warn` is a native dry run); everything sbopkg
//! installs is a regular Slackware package tagged `_SBo` in its build
//! field, so the installed list and info come from the pkgtools database
//! filtered to that tag. `-i` (build+install) has no simulate mode and is
//! driven one package at a time; `-B` is the documented batch
//! (non-interactive) switch; `-r` rsyncs the local repo copy. `-s` search
//! output is parsed as its `category/name` match lines.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "sbopkg";
const PROGRAMS: &[&str] = &["sbopkg"];
const DATABASE_DIRS: [&str; 2] = ["/var/lib/pkgtools/packages", "/var/log/packages"];

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
}

/// SlackBuilds carry one version per repo checkout; there is no syntax to
/// build an older one.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but sbopkg builds whatever the SlackBuilds tree carries"
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
        "sbopkg"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "slackware"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::SEARCH | Capabilities::REFRESH
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
        for package in packages {
            let mut cmd = Cmd::new(&self.program).elevated(true);
            if ctx.assume_yes {
                cmd = cmd.arg("-B");
            }
            self.run(cmd.arg("-i").arg(&package.name), ctx).await?;
        }
        Ok(())
    }

    /// sbopkg has no remove verb; what it installed is a plain Slackware
    /// package, so removal goes through pkgtools' removepkg.
    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let removepkg = find_program("removepkg").ok_or_else(|| {
            Error::Other(format!(
                "{ID}: removal is delegated to `removepkg`, which was not found on PATH"
            ))
        })?;
        let mut cmd = Cmd::new(removepkg).elevated(true);
        if ctx.dry_run {
            cmd = cmd.arg("-warn");
        }
        cmd = cmd.args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(read_installed(database_dir())?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        if let Some((path, mut package)) = find_entry(database_dir(), name)? {
            let details = parse_entry_file(&std::fs::read_to_string(path)?);
            package.description = details.description;
            package.download_size = details.download_size;
            package.installed_size = details.installed_size;
            return Ok(Box::new(package));
        }
        // Not installed: the repo search at least confirms the build exists
        // and names its category.
        let output = self
            .query()
            .arg("-s")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        parse_search(&output.stdout)
            .into_iter()
            .find(|package| package.name == name)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("-s")
            .arg(query)
            .capture(&self.elevator, None)
            .await?;
        let packages = parse_search(&output.stdout);
        if packages.is_empty() {
            output.require_success()?;
        }
        Ok(packages
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("refresh"));
        }
        self.run(Cmd::new(&self.program).arg("-r").elevated(true), ctx)
            .await
    }
}

/// The installed-package database directory, preferring the modern
/// location over the pre-15.0 one.
fn database_dir() -> &'static Path {
    DATABASE_DIRS
        .iter()
        .map(Path::new)
        .find(|dir| dir.is_dir())
        .unwrap_or_else(|| Path::new(DATABASE_DIRS[0]))
}

/// The SlackBuilds.org subset of the database: entries whose build field
/// carries the `_SBo` tag, sorted by name.
fn read_installed(dir: &Path) -> Result<Vec<SbopkgPackage>> {
    let mut packages = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let file_name = entry?.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if let Some(package) = parse_entry_name(file_name) {
            packages.push(package);
        }
    }
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(packages)
}

/// Locate `name`'s `_SBo` database entry, returning its path and parsed
/// identity.
fn find_entry(dir: &Path, name: &str) -> Result<Option<(PathBuf, SbopkgPackage)>> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if let Some(package) = parse_entry_name(file_name)
            && package.name == name
        {
            return Ok(Some((entry.path(), package)));
        }
    }
    Ok(None)
}

/// A database entry name is `name-version-arch-build`, split from the right
/// because names may contain hyphens; only `_SBo`-tagged builds belong to
/// this backend.
fn parse_entry_name(entry: &str) -> Option<SbopkgPackage> {
    let mut fields = entry.rsplitn(4, '-');
    let build = fields.next()?;
    let arch = fields.next()?;
    let version = fields.next()?;
    let name = fields.next()?;
    if name.is_empty() || version.is_empty() || arch.is_empty() || !build.ends_with("_SBo") {
        return None;
    }
    Some(SbopkgPackage {
        name: name.to_string(),
        version: Some(version.to_string()),
        architecture: Some(arch.to_string()),
        state: InstallState::Installed,
        ..Default::default()
    })
}

#[derive(Default)]
struct EntryDetails {
    download_size: Option<u64>,
    installed_size: Option<u64>,
    description: Option<String>,
}

/// A database entry file: `KEY: value` header lines, then a
/// `PACKAGE DESCRIPTION:` stanza of `name: text` lines ending at
/// `FILE LIST:`; only the first stanza line (the summary) is kept.
fn parse_entry_file(contents: &str) -> EntryDetails {
    let mut details = EntryDetails::default();
    let mut in_description = false;
    for line in contents.lines() {
        if line.starts_with("FILE LIST:") {
            break;
        }
        if in_description {
            if let Some((_, text)) = line.split_once(':') {
                let text = text.trim();
                if !text.is_empty() && details.description.is_none() {
                    details.description = Some(text.to_string());
                }
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "COMPRESSED PACKAGE SIZE" => details.download_size = parse_size(value),
            "UNCOMPRESSED PACKAGE SIZE" => details.installed_size = parse_size(value),
            "PACKAGE DESCRIPTION" => in_description = true,
            _ => {}
        }
    }
    details
}

/// Sizes as pkgtools writes them: `461K`, `1.6M`, or the older `461 K`.
fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let unit_at = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let number: f64 = text[..unit_at].parse().ok()?;
    let factor = match text[unit_at..].trim() {
        "" | "B" => 1.0,
        "K" | "KB" => 1024.0,
        "M" | "MB" => 1024.0 * 1024.0,
        "G" | "GB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((number * factor) as u64)
}

/// `-s`: matches print as bare `category/name` lines; narration lines
/// contain whitespace and fall out.
fn parse_search(stdout: &str) -> Vec<SbopkgPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.contains(char::is_whitespace) {
                return None;
            }
            let (category, name) = line.split_once('/')?;
            if category.is_empty() || name.is_empty() || name.contains('/') {
                return None;
            }
            Some(SbopkgPackage {
                name: name.to_string(),
                origin: Some(category.to_string()),
                state: InstallState::Available,
                ..Default::default()
            })
        })
        .collect()
}

/// A package as sbopkg (and the pkgtools database behind it) describes it.
#[derive(Debug, Default)]
pub struct SbopkgPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub architecture: Option<String>,
    pub origin: Option<String>,
    pub installed_size: Option<u64>,
    pub download_size: Option<u64>,
    pub state: InstallState,
}

impl Package for SbopkgPackage {
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

    fn architecture(&self) -> Option<&str> {
        self.architecture.as_deref()
    }

    fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
    }

    fn installed_size(&self) -> Option<u64> {
        self.installed_size
    }

    fn download_size(&self) -> Option<u64> {
        self.download_size
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_sbo_tagged_entries_belong_to_this_backend() {
        let package = parse_entry_name("vlc-3.0.18-x86_64-1_SBo").unwrap();
        assert_eq!(package.name, "vlc");
        assert_eq!(package.version.as_deref(), Some("3.0.18"));
        assert_eq!(package.architecture.as_deref(), Some("x86_64"));
        assert_eq!(package.state, InstallState::Installed);

        assert!(parse_entry_name("xz-5.4.4-x86_64-1").is_none());
        assert!(parse_entry_name("broken-1_SBo").is_none());
    }

    #[test]
    fn hyphenated_names_split_right_to_left() {
        let package = parse_entry_name("brave-browser-1.62.156-x86_64-1_SBo").unwrap();
        assert_eq!(package.name, "brave-browser");
        assert_eq!(package.version.as_deref(), Some("1.62.156"));
    }

    #[test]
    fn parses_search_match_lines() {
        let stdout = "\
Searching for vlc
Found the following matches for vlc:

multimedia/vlc
network/vlc-remote
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "vlc");
        assert_eq!(packages[0].origin.as_deref(), Some("multimedia"));
        assert_eq!(packages[0].state, InstallState::Available);
        assert_eq!(packages[1].name, "vlc-remote");
    }

    #[test]
    fn parses_entry_files() {
        let contents = "\
PACKAGE NAME:     vlc-3.0.18-x86_64-1_SBo
COMPRESSED PACKAGE SIZE:     58M
UNCOMPRESSED PACKAGE SIZE:     220M
PACKAGE LOCATION: /tmp/vlc-3.0.18-x86_64-1_SBo.tgz
PACKAGE DESCRIPTION:
vlc: vlc (multimedia player and streamer)
vlc:
FILE LIST:
./
";
        let details = parse_entry_file(contents);
        assert_eq!(details.download_size, Some(58 * 1024 * 1024));
        assert_eq!(details.installed_size, Some(220 * 1024 * 1024));
        assert_eq!(
            details.description.as_deref(),
            Some("vlc (multimedia player and streamer)")
        );
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("58M"), Some(58 * 1024 * 1024));
        assert_eq!(parse_size("461 K"), Some(461 * 1024));
        assert_eq!(parse_size("garbage"), None);
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("vlc@3.0.18")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("vlc")]).is_ok());
    }
}
