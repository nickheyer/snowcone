//! netpkg backend for snowcone.
//!
//! Zenwalk's netpkg is an interactive, dialog-driven script, so this
//! backend is deliberately conservative: `netpkg <name>` (install-or-
//! upgrade from the configured mirror) and `netpkg upgrade` are the only
//! CLI verbs it trusts, and it never invents flags - netpkg has no yes
//! switch, so prompts pass through to the terminal, and no simulate mode,
//! so dry runs error. Everything readable comes from the pkgtools database
//! Zenwalk shares with Slackware: the installed list, info, and - because
//! netpkg's own package picker is a dialog with no parseable output - a
//! LOCAL, INSTALLED-ONLY search fallback. Removal delegates to pkgtools'
//! `removepkg` (with its native `-warn` dry run); netpkg itself has no
//! defensible remove verb.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "netpkg";
const PROGRAMS: &[&str] = &["netpkg"];
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

/// netpkg installs whatever the configured mirror currently carries; there
/// is no syntax to ask for an older version.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but netpkg only installs the mirror's current tree"
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
        "netpkg"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "slackware"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::SEARCH | Capabilities::UPGRADE
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
            let cmd = Cmd::new(&self.program).arg(&package.name).elevated(true);
            self.run(cmd, ctx).await?;
        }
        Ok(())
    }

    /// netpkg has no defensible remove verb; what it installed is a plain
    /// Slackware package, so removal goes through pkgtools' removepkg.
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

    /// Info reads the database entry of an installed package; netpkg has no
    /// parseable repository-side metadata output.
    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let (path, mut package) = find_entry(database_dir(), name)?
            .ok_or_else(|| Error::NotFound(name.to_string()))?;
        let details = parse_entry_file(&std::fs::read_to_string(path)?);
        package.description = details.description;
        package.download_size = details.download_size;
        package.installed_size = details.installed_size;
        Ok(Box::new(package))
    }

    /// netpkg's own search is an interactive dialog with no parseable
    /// output, so this searches the local database - installed packages
    /// only.
    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        Ok(read_installed(database_dir())?
            .into_iter()
            .filter(|package| matches(&package.name, query))
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        if packages.is_empty() {
            return self
                .run(Cmd::new(&self.program).arg("upgrade").elevated(true), ctx)
                .await;
        }
        // Reinstalling by name pulls the mirror's current version - the
        // closest thing netpkg has to a targeted upgrade.
        for package in packages {
            let cmd = Cmd::new(&self.program).arg(&package.name).elevated(true);
            self.run(cmd, ctx).await?;
        }
        Ok(())
    }
}

/// Case-insensitive substring match, the local stand-in for a search verb.
fn matches(name: &str, query: &str) -> bool {
    name.to_ascii_lowercase()
        .contains(&query.to_ascii_lowercase())
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

/// Every database entry, one installed package each, sorted by name
/// (directory order is arbitrary).
fn read_installed(dir: &Path) -> Result<Vec<NetpkgPackage>> {
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

/// Locate `name`'s database entry, returning its path and parsed identity.
fn find_entry(dir: &Path, name: &str) -> Result<Option<(PathBuf, NetpkgPackage)>> {
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
/// because package names may contain hyphens (`gcc-g++`).
fn parse_entry_name(entry: &str) -> Option<NetpkgPackage> {
    let mut fields = entry.rsplitn(4, '-');
    let build = fields.next()?;
    let arch = fields.next()?;
    let version = fields.next()?;
    let name = fields.next()?;
    if name.is_empty() || version.is_empty() || arch.is_empty() || build.is_empty() {
        return None;
    }
    Some(NetpkgPackage {
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

/// A package as the pkgtools database behind netpkg describes it.
#[derive(Debug, Default)]
pub struct NetpkgPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub architecture: Option<String>,
    pub installed_size: Option<u64>,
    pub download_size: Option<u64>,
    pub state: InstallState,
}

impl Package for NetpkgPackage {
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
    fn splits_entry_names_right_to_left() {
        let package = parse_entry_name("gcc-g++-13.2.0-x86_64-1").unwrap();
        assert_eq!(package.name, "gcc-g++");
        assert_eq!(package.version.as_deref(), Some("13.2.0"));
        assert_eq!(package.architecture.as_deref(), Some("x86_64"));
        assert_eq!(package.state, InstallState::Installed);

        assert!(parse_entry_name("broken-1.0").is_none());
        assert!(parse_entry_name("noversion").is_none());
    }

    #[test]
    fn parses_entry_files() {
        let contents = "\
PACKAGE NAME:     xz-5.4.4-x86_64-1
COMPRESSED PACKAGE SIZE:     461K
UNCOMPRESSED PACKAGE SIZE:     1.6M
PACKAGE DESCRIPTION:
xz: xz (compression utility based on the LZMA algorithm)
FILE LIST:
./
";
        let details = parse_entry_file(contents);
        assert_eq!(details.download_size, Some(461 * 1024));
        assert_eq!(details.installed_size, Some((1.6 * 1024.0 * 1024.0) as u64));
        assert_eq!(
            details.description.as_deref(),
            Some("xz (compression utility based on the LZMA algorithm)")
        );
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("461K"), Some(461 * 1024));
        assert_eq!(parse_size("461 K"), Some(461 * 1024));
        assert_eq!(parse_size("garbage"), None);
    }

    #[test]
    fn search_matches_are_case_insensitive_substrings() {
        assert!(matches("mozilla-nss", "NSS"));
        assert!(matches("xz", "xz"));
        assert!(!matches("xz", "nano"));
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("xz@5.4.4")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("xz")]).is_ok());
    }
}
