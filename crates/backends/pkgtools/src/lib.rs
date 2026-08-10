//! Slackware pkgtools backend for snowcone.
//!
//! Drives the low-level `installpkg`/`removepkg` pair. installpkg's
//! contract is a *path to a package file* (`.tgz`/`.txz`), not a repository
//! name - there is no repository side at all, which is why only the core
//! capabilities are advertised. The installed database is a directory of
//! entry files named `name-version-arch-build` (Slackware 15.0 keeps it in
//! /var/lib/pkgtools/packages and leaves /var/log/packages as a symlink to
//! it, so the modern path is preferred); package names may contain hyphens,
//! so entries split right-to-left on the last three. Neither tool prompts,
//! so `assume_yes` has nothing to do. Dry runs are native but spelled
//! differently per the man pages: `--warn` for installpkg, `-warn` for
//! removepkg. Reads are plain file reads: no subprocess, no elevation.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "pkgtools";
const PROGRAMS: &[&str] = &["installpkg"];
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
}

/// installpkg installs exactly the package file it is given; a `@version`
/// suffix has nothing to resolve against.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but installpkg installs exactly the package file it is given"
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
        "Slackware pkgtools"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "slackware"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
    }

    fn needs_elevation(&self, operation: Operation) -> bool {
        matches!(
            operation,
            Operation::Install | Operation::Remove | Operation::Upgrade | Operation::Refresh
        )
    }

    /// Package arguments are *paths to package files* (`.tgz`/`.txz`) -
    /// that is installpkg's contract, not a defect of this backend.
    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        let mut cmd = Cmd::new(&self.program).elevated(true);
        if ctx.dry_run {
            cmd = cmd.arg("--warn");
        }
        cmd = cmd.args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let removepkg = find_program("removepkg")
            .ok_or_else(|| Error::Other(format!("{ID}: `removepkg` not found on PATH")))?;
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
        let (path, mut package) =
            find_entry(database_dir(), name)?.ok_or_else(|| Error::NotFound(name.to_string()))?;
        let details = parse_entry_file(&std::fs::read_to_string(path)?);
        package.description = details.description;
        package.download_size = details.download_size;
        package.installed_size = details.installed_size;
        Ok(Box::new(package))
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

/// Every database entry, one installed package each, sorted by name
/// (directory order is arbitrary).
fn read_installed(dir: &Path) -> Result<Vec<PkgtoolsPackage>> {
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
fn find_entry(dir: &Path, name: &str) -> Result<Option<(PathBuf, PkgtoolsPackage)>> {
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

/// A database entry name is `name-version-arch-build`; anything without all
/// four fields is not a package entry.
fn parse_entry_name(entry: &str) -> Option<PkgtoolsPackage> {
    let (name, version, arch, _build) = split_entry(entry)?;
    Some(PkgtoolsPackage {
        name,
        version: Some(version),
        architecture: Some(arch),
        state: InstallState::Installed,
        ..Default::default()
    })
}

/// Split `name-version-arch-build` from the right - package names may
/// contain hyphens (`gcc-g++`), the last three fields may not.
fn split_entry(entry: &str) -> Option<(String, String, String, String)> {
    let mut fields = entry.rsplitn(4, '-');
    let build = fields.next()?;
    let arch = fields.next()?;
    let version = fields.next()?;
    let name = fields.next()?;
    if name.is_empty() || version.is_empty() || arch.is_empty() || build.is_empty() {
        return None;
    }
    Some((
        name.to_string(),
        version.to_string(),
        arch.to_string(),
        build.to_string(),
    ))
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

/// A package as the pkgtools database describes it.
#[derive(Debug, Default)]
pub struct PkgtoolsPackage {
    pub name: String,
    pub version: Option<String>,
    pub architecture: Option<String>,
    pub description: Option<String>,
    pub installed_size: Option<u64>,
    pub download_size: Option<u64>,
    pub state: InstallState,
}

impl Package for PkgtoolsPackage {
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
        let (name, version, arch, build) = split_entry("xz-5.4.4-x86_64-1").unwrap();
        assert_eq!(name, "xz");
        assert_eq!(version, "5.4.4");
        assert_eq!(arch, "x86_64");
        assert_eq!(build, "1");
    }

    #[test]
    fn hyphenated_names_keep_their_hyphens() {
        let (name, version, ..) = split_entry("gcc-g++-13.2.0-x86_64-1").unwrap();
        assert_eq!(name, "gcc-g++");
        assert_eq!(version, "13.2.0");

        let (name, version, ..) = split_entry("mozilla-nss-3.101-x86_64-1").unwrap();
        assert_eq!(name, "mozilla-nss");
        assert_eq!(version, "3.101");
    }

    #[test]
    fn build_tags_with_suffixes_stay_in_the_build_field() {
        let (name, version, arch, build) = split_entry("vlc-3.0.18-x86_64-1_SBo").unwrap();
        assert_eq!(name, "vlc");
        assert_eq!(version, "3.0.18");
        assert_eq!(arch, "x86_64");
        assert_eq!(build, "1_SBo");

        let (name, _, _, build) = split_entry("aaa_base-15.0-x86_64-3_slack15.0").unwrap();
        assert_eq!(name, "aaa_base");
        assert_eq!(build, "3_slack15.0");
    }

    #[test]
    fn rejects_malformed_entry_names() {
        assert!(split_entry("broken-1.0").is_none());
        assert!(split_entry("noversion").is_none());
        assert!(split_entry("trailing-1.0-x86_64-").is_none());
    }

    #[test]
    fn parses_entry_names_as_installed_packages() {
        let package = parse_entry_name("xz-5.4.4-x86_64-1").unwrap();
        assert_eq!(package.name, "xz");
        assert_eq!(package.version.as_deref(), Some("5.4.4"));
        assert_eq!(package.architecture.as_deref(), Some("x86_64"));
        assert_eq!(package.state, InstallState::Installed);
    }

    #[test]
    fn parses_entry_files() {
        let contents = "\
PACKAGE NAME:     xz-5.4.4-x86_64-1
COMPRESSED PACKAGE SIZE:     461K
UNCOMPRESSED PACKAGE SIZE:     1.6M
PACKAGE LOCATION: /var/log/mount/slackware64/a/xz-5.4.4-x86_64-1.txz
PACKAGE DESCRIPTION:
xz: xz (compression utility based on the LZMA algorithm)
xz:
xz: xz provides very high compression ratios.
FILE LIST:
./
install/
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
    fn parses_sizes_in_every_historical_spelling() {
        assert_eq!(parse_size("461K"), Some(461 * 1024));
        assert_eq!(parse_size("461 K"), Some(461 * 1024));
        assert_eq!(parse_size("1.6M"), Some((1.6 * 1024.0 * 1024.0) as u64));
        assert_eq!(parse_size("12345"), Some(12345));
        assert_eq!(parse_size("unknown"), None);
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("foo-1.0.txz@1.0")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("/tmp/foo-1.0-x86_64-1.txz")]).is_ok());
    }
}
