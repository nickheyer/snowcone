//! slpkg backend for snowcone.
//!
//! slpkg's verbs have been stable since the 4.x rewrite (`update`,
//! `upgrade`, `install`, `remove`, `search`, `--yes`), but its *output* is
//! a colorized table that shifts between major versions - so parsers strip
//! ANSI codes and accept only lines carrying a real package token, the
//! installed list and info come straight from the pkgtools database slpkg
//! manages, and the outdated listing leans on `upgrade --check` (modern
//! slpkg). Nothing has a simulate mode, so dry runs error instead of
//! acting. There is no targeted upgrade verb - installing an installed
//! package pulls the newest version the repositories carry.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "slpkg";
const PROGRAMS: &[&str] = &["slpkg"];
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

    /// Shared flags for mutating commands: elevated, with `--yes` when
    /// prompts should answer themselves.
    fn mutation(&self, verb: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = Cmd::new(&self.program).arg(verb).elevated(true);
        if ctx.assume_yes {
            cmd = cmd.arg("--yes");
        }
        cmd
    }

    fn no_dry_run(&self, operation: &str) -> Error {
        Error::Other(format!("{ID}: {operation} has no dry-run mode"))
    }
}

/// slpkg installs whatever its repositories currently carry; there is no
/// pkg=ver syntax.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but slpkg only installs what its repositories carry"
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
        "slpkg"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "slackware"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::REFRESH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
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
            .mutation("install", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        let cmd = self
            .mutation("remove", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    /// slpkg's own listing is a version-shifting table; the pkgtools
    /// database it manages is the authoritative installed set.
    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(read_installed(database_dir())?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    /// Repository-side metadata printing is version-unstable, so info reads
    /// the database entry of an installed package instead.
    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let (path, mut package) =
            find_entry(database_dir(), name)?.ok_or_else(|| Error::NotFound(name.to_string()))?;
        let details = parse_entry_file(&std::fs::read_to_string(path)?);
        package.description = details.description;
        package.download_size = details.download_size;
        package.installed_size = details.installed_size;
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("search")
            .arg(query)
            .capture(&self.elevator, None)
            .await?;
        let mut packages = parse_search(&output.stdout);
        if packages.is_empty() {
            output.require_success()?;
        }
        // The search table says nothing certain about the local install;
        // the pkgtools database does.
        let installed = read_installed(database_dir()).unwrap_or_default();
        mark_installed(&mut packages, &installed);
        Ok(packages
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("refresh"));
        }
        self.run(self.mutation("update", ctx), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        let cmd = if packages.is_empty() {
            self.mutation("upgrade", ctx)
        } else {
            // Reinstalling an installed package pulls the newest version -
            // slpkg's targeted upgrade.
            self.mutation("install", ctx)
                .args(packages.iter().map(|package| package.name.as_str()))
        };
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["upgrade", "--check"])
            .capture(&self.elevator, None)
            .await?;
        let packages = parse_check(&output.stdout);
        if packages.is_empty() {
            output.require_success()?;
        }
        Ok(packages
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }
}

/// Fill in install state by name against the local database: same version
/// means installed, a different one means the repository is ahead.
fn mark_installed(packages: &mut [SlpkgPackage], installed: &[SlpkgPackage]) {
    for package in packages {
        let Some(local) = installed.iter().find(|local| local.name == package.name) else {
            continue;
        };
        if package.version.is_none() || package.version == local.version {
            package.state = InstallState::Installed;
            package.version = local.version.clone();
        } else {
            package.state = InstallState::Upgradable;
            package.latest_version = package.version.take();
            package.version = local.version.clone();
        }
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
fn read_installed(dir: &Path) -> Result<Vec<SlpkgPackage>> {
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
fn find_entry(dir: &Path, name: &str) -> Result<Option<(PathBuf, SlpkgPackage)>> {
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
fn parse_entry_name(entry: &str) -> Option<SlpkgPackage> {
    let mut fields = entry.rsplitn(4, '-');
    let build = fields.next()?;
    let arch = fields.next()?;
    let version = fields.next()?;
    let name = fields.next()?;
    if name.is_empty() || version.is_empty() || arch.is_empty() || build.is_empty() {
        return None;
    }
    Some(SlpkgPackage {
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

/// `slpkg search`: colorized rows whose exact layout shifts across major
/// versions - a line counts only when (after ANSI stripping and an optional
/// `repo:` prefix) it leads with a real package token; narration lines
/// carry no such token and fall out.
fn parse_search(stdout: &str) -> Vec<SlpkgPackage> {
    stdout
        .lines()
        .filter_map(|raw| {
            let line = strip_ansi(raw);
            let mut tokens = line.split_whitespace();
            let first = tokens.next()?;
            let (origin, token) = match first.strip_suffix(':') {
                Some(repo) if !repo.is_empty() => (Some(repo.to_string()), tokens.next()?),
                _ => (None, first),
            };
            let (name, version, arch) = split_token(token)?;
            Some(SlpkgPackage {
                name,
                version: Some(version),
                architecture: arch,
                origin,
                state: InstallState::Available,
                ..Default::default()
            })
        })
        .collect()
}

/// `slpkg upgrade --check`: only `installed -> candidate` arrow lines are
/// trusted; the candidate side may be a full package token or a bare
/// version.
fn parse_check(stdout: &str) -> Vec<SlpkgPackage> {
    stdout
        .lines()
        .filter_map(|raw| {
            let line = strip_ansi(raw);
            let (left, right) = line.split_once("->")?;
            let left_token = left.split_whitespace().next_back()?;
            let right_token = right.split_whitespace().next()?;
            let (name, version, arch) = split_token(left_token)?;
            let latest = match split_token(right_token) {
                Some((_, latest, _)) => latest,
                None if right_token.starts_with(|c: char| c.is_ascii_digit()) => {
                    right_token.to_string()
                }
                None => return None,
            };
            Some(SlpkgPackage {
                name,
                version: Some(version),
                latest_version: Some(latest),
                architecture: arch,
                state: InstallState::Upgradable,
                ..Default::default()
            })
        })
        .collect()
}

/// A package token: the full Slackware `name-version-arch-build` triplet
/// when the version and build fields lead with digits, else nix's
/// convention that the version starts at the first dash followed by a
/// digit (`nano-7.2`); anything without a version is not a package token.
fn split_token(token: &str) -> Option<(String, String, Option<String>)> {
    let fields: Vec<&str> = token.rsplitn(4, '-').collect();
    if let [build, arch, version, name] = fields.as_slice()
        && !name.is_empty()
        && version.starts_with(|c: char| c.is_ascii_digit())
        && build.starts_with(|c: char| c.is_ascii_digit())
        && !arch.is_empty()
    {
        return Some((
            (*name).to_string(),
            (*version).to_string(),
            Some((*arch).to_string()),
        ));
    }
    for (idx, _) in token.match_indices('-') {
        if token[idx + 1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
        {
            return Some((token[..idx].to_string(), token[idx + 1..].to_string(), None));
        }
    }
    None
}

/// Remove ANSI escape sequences (slpkg colorizes everything).
fn strip_ansi(text: &str) -> String {
    let mut cleaned = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            cleaned.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            chars.next();
        }
    }
    cleaned
}

/// A package as slpkg describes it.
#[derive(Debug, Default)]
pub struct SlpkgPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub architecture: Option<String>,
    pub origin: Option<String>,
    pub installed_size: Option<u64>,
    pub download_size: Option<u64>,
    pub state: InstallState,
}

impl Package for SlpkgPackage {
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
    fn splits_full_triplets_and_short_tokens() {
        let (name, version, arch) = split_token("mozilla-nss-3.101-x86_64-1").unwrap();
        assert_eq!(name, "mozilla-nss");
        assert_eq!(version, "3.101");
        assert_eq!(arch.as_deref(), Some("x86_64"));

        let (name, version, arch) = split_token("nano-7.2").unwrap();
        assert_eq!(name, "nano");
        assert_eq!(version, "7.2");
        assert_eq!(arch, None);

        assert!(split_token("narration").is_none());
        assert!(split_token("The").is_none());
    }

    #[test]
    fn parses_search_rows_and_skips_narration() {
        let stdout = "\
The list below shows the repo packages:

sbo: nano-7.2
slack: mozilla-nss-3.101-x86_64-1

Total found 2 packages.
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "nano");
        assert_eq!(packages[0].version.as_deref(), Some("7.2"));
        assert_eq!(packages[0].origin.as_deref(), Some("sbo"));
        assert_eq!(packages[1].name, "mozilla-nss");
        assert_eq!(packages[1].architecture.as_deref(), Some("x86_64"));
        assert_eq!(packages[1].origin.as_deref(), Some("slack"));
    }

    #[test]
    fn parses_colorized_output() {
        let stdout = "\u{1b}[1;32msbo:\u{1b}[0m nano-7.2\n";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "nano");
    }

    #[test]
    fn parses_upgrade_check_arrows() {
        let stdout = "\
The list below shows the packages that will be upgraded:

nano-7.2 -> 8.0
mozilla-nss-3.101-x86_64-1 -> mozilla-nss-3.107-x86_64-1
";
        let packages = parse_check(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "nano");
        assert_eq!(packages[0].version.as_deref(), Some("7.2"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("8.0"));
        assert_eq!(packages[1].latest_version.as_deref(), Some("3.107"));
        assert!(packages.iter().all(|p| p.state == InstallState::Upgradable));
    }

    #[test]
    fn marks_search_results_against_the_local_database() {
        let mut packages = vec![
            SlpkgPackage {
                name: "nano".to_string(),
                version: Some("8.0".to_string()),
                state: InstallState::Available,
                ..Default::default()
            },
            SlpkgPackage {
                name: "xz".to_string(),
                version: Some("5.4.4".to_string()),
                state: InstallState::Available,
                ..Default::default()
            },
        ];
        let installed = vec![
            SlpkgPackage {
                name: "nano".to_string(),
                version: Some("7.2".to_string()),
                ..Default::default()
            },
            SlpkgPackage {
                name: "xz".to_string(),
                version: Some("5.4.4".to_string()),
                ..Default::default()
            },
        ];
        mark_installed(&mut packages, &installed);
        assert_eq!(packages[0].state, InstallState::Upgradable);
        assert_eq!(packages[0].version.as_deref(), Some("7.2"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("8.0"));
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn parses_database_entry_names() {
        let package = parse_entry_name("gcc-g++-13.2.0-x86_64-1").unwrap();
        assert_eq!(package.name, "gcc-g++");
        assert_eq!(package.version.as_deref(), Some("13.2.0"));
        assert_eq!(package.state, InstallState::Installed);
        assert!(parse_entry_name("broken-1.0").is_none());
    }

    #[test]
    fn parses_entry_files_and_sizes() {
        let contents = "\
PACKAGE NAME:     nano-7.2-x86_64-1
COMPRESSED PACKAGE SIZE:     650K
UNCOMPRESSED PACKAGE SIZE:     2.8M
PACKAGE DESCRIPTION:
nano: nano (a pico-like text editor)
FILE LIST:
./
";
        let details = parse_entry_file(contents);
        assert_eq!(details.download_size, Some(650 * 1024));
        assert_eq!(details.installed_size, Some((2.8 * 1024.0 * 1024.0) as u64));
        assert_eq!(
            details.description.as_deref(),
            Some("nano (a pico-like text editor)")
        );
        assert_eq!(parse_size("461 K"), Some(461 * 1024));
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("nano@7.2")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("nano")]).is_ok());
    }
}
