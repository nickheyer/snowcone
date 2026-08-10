//! slackpkg backend for snowcone.
//!
//! slackpkg wraps pkgtools with mirror awareness, but the script is built
//! for humans: `-batch=on -default_answer=y` (that is the documented
//! spelling) is the only way to keep it from prompting, its messages are
//! gettext-translated (hence `LC_ALL=C` on every parsed read), and nothing
//! has a simulate mode, so dry runs error instead of acting. There is no
//! list-installed verb - the installed set is read straight from the
//! pkgtools database slackpkg manages (/var/lib/pkgtools/packages, with the
//! pre-15.0 /var/log/packages fallback). Search rows changed shape in 15.0
//! (three bracketed columns instead of the old `[ status ] - package`
//! line); both forms are parsed.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "slackpkg";
const PROGRAMS: &[&str] = &["slackpkg"];
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

    /// Shared shape for mutating commands: elevated, with the batch flags
    /// (which must precede the verb) when prompts should answer themselves.
    fn mutation(&self, ctx: &OpContext) -> Cmd {
        let mut cmd = Cmd::new(&self.program).elevated(true);
        if ctx.assume_yes {
            cmd = cmd.args(["-batch=on", "-default_answer=y"]);
        }
        cmd
    }

    fn no_dry_run(&self, operation: &str) -> Error {
        Error::Other(format!("{ID}: {operation} has no dry-run mode"))
    }
}

/// slackpkg installs whatever the configured mirror currently carries;
/// there is no syntax to ask for an older version.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but slackpkg only installs the mirror's current tree"
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
        "slackpkg"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "slackware"
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
            .mutation(ctx)
            .arg("install")
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        let cmd = self
            .mutation(ctx)
            .arg("remove")
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    /// slackpkg has no list verb; the pkgtools database it manages is the
    /// authoritative installed set.
    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(read_installed(database_dir())?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
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
        let stanzas = parse_info(&output.stdout);
        let mut package = stanzas
            .into_iter()
            .find(|package| package.name == name)
            .ok_or_else(|| Error::NotFound(name.to_string()))?;
        // `slackpkg info` reads PACKAGES.TXT and says nothing about the
        // local install; the pkgtools database fills that in.
        if let Some(installed) = read_installed(database_dir())
            .unwrap_or_default()
            .into_iter()
            .find(|installed| installed.name == package.name)
        {
            if installed.version == package.version {
                package.state = InstallState::Installed;
            } else {
                package.state = InstallState::Upgradable;
                package.latest_version = package.version.take();
                package.version = installed.version;
            }
        }
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("search")
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
        self.run(self.mutation(ctx).arg("update"), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        let cmd = if packages.is_empty() {
            self.mutation(ctx).arg("upgrade-all")
        } else {
            self.mutation(ctx)
                .arg("upgrade")
                .args(packages.iter().map(|package| package.name.as_str()))
        };
        self.run(cmd, ctx).await
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
fn read_installed(dir: &Path) -> Result<Vec<SlackpkgPackage>> {
    let mut packages = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let file_name = entry?.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some((name, version, arch, _build)) = split_entry(file_name) else {
            continue;
        };
        packages.push(SlackpkgPackage {
            name,
            version: Some(version),
            architecture: Some(arch),
            state: InstallState::Installed,
            ..Default::default()
        });
    }
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(packages)
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

/// `slackpkg search`: one row per match; only rows whose status is a known
/// package state survive, which also drops the 15.0 column-header row.
fn parse_search(stdout: &str) -> Vec<SlackpkgPackage> {
    stdout.lines().filter_map(parse_search_line).collect()
}

/// One search row: 15.0 prints three bracketed columns
/// (status/repository/package); 14.x printed `[ status ] - package`.
fn parse_search_line(line: &str) -> Option<SlackpkgPackage> {
    let groups = bracket_groups(line);
    let (status, origin, entry) = match groups.as_slice() {
        [status, origin, entry, ..] => (*status, Some((*origin).to_string()), *entry),
        [status] => {
            let after = line.rsplit(']').next()?;
            (*status, None, after.trim().trim_start_matches('-').trim())
        }
        _ => return None,
    };
    let state = match status {
        "installed" => InstallState::Installed,
        "uninstalled" => InstallState::Available,
        status if status.contains("upgrade") => InstallState::Upgradable,
        _ => return None,
    };
    let (name, version, arch, _build) = split_entry(entry)?;
    let mut package = SlackpkgPackage {
        name,
        architecture: Some(arch),
        origin,
        state,
        ..Default::default()
    };
    // An upgradable row names the repository's version, not the installed
    // one.
    if state == InstallState::Upgradable {
        package.latest_version = Some(version);
    } else {
        package.version = Some(version);
    }
    Some(package)
}

/// All `[...]` groups on a line, trimmed.
fn bracket_groups(line: &str) -> Vec<&str> {
    let mut groups = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find('[') {
        let Some(len) = rest[start + 1..].find(']') else {
            break;
        };
        groups.push(rest[start + 1..start + 1 + len].trim());
        rest = &rest[start + 1 + len + 1..];
    }
    groups
}

/// `slackpkg info`: PACKAGES.TXT stanzas - `PACKAGE KEY: value` lines with
/// a `name: text` description block after `PACKAGE DESCRIPTION:`; only the
/// summary (first description line) is kept.
fn parse_info(stdout: &str) -> Vec<SlackpkgPackage> {
    let mut packages: Vec<SlackpkgPackage> = Vec::new();
    let mut in_description = false;
    for line in stdout.lines() {
        if let Some(value) = line.strip_prefix("PACKAGE NAME:") {
            in_description = false;
            let entry = strip_package_extension(value.trim());
            if let Some((name, version, arch, _build)) = split_entry(entry) {
                packages.push(SlackpkgPackage {
                    name,
                    version: Some(version),
                    architecture: Some(arch),
                    state: InstallState::Available,
                    ..Default::default()
                });
            }
            continue;
        }
        let Some(package) = packages.last_mut() else {
            continue;
        };
        if in_description {
            if let Some((_, text)) = line.split_once(':') {
                let text = text.trim();
                if !text.is_empty() && package.description.is_none() {
                    package.description = Some(text.to_string());
                }
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "PACKAGE LOCATION" => {
                package.origin =
                    Some(value.trim_start_matches("./").to_string()).filter(|v| !v.is_empty());
            }
            "PACKAGE SIZE (compressed)" => package.download_size = parse_size(value),
            "PACKAGE SIZE (uncompressed)" => package.installed_size = parse_size(value),
            "PACKAGE DESCRIPTION" => in_description = true,
            _ => {}
        }
    }
    packages
}

/// Drop the package-tarball extension PACKAGES.TXT carries on names.
fn strip_package_extension(entry: &str) -> &str {
    for extension in [".txz", ".tgz", ".tbz", ".tlz"] {
        if let Some(stripped) = entry.strip_suffix(extension) {
            return stripped;
        }
    }
    entry
}

/// Sizes as PACKAGES.TXT writes them: `461 K`, `1.6M`, plain kilobyte-ish.
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

/// A package as slackpkg describes it.
#[derive(Debug, Default)]
pub struct SlackpkgPackage {
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

impl Package for SlackpkgPackage {
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
    fn splits_hyphenated_entry_names_right_to_left() {
        let (name, version, arch, build) = split_entry("gcc-g++-13.2.0-x86_64-1").unwrap();
        assert_eq!(name, "gcc-g++");
        assert_eq!(version, "13.2.0");
        assert_eq!(arch, "x86_64");
        assert_eq!(build, "1");
        assert!(split_entry("broken-1.0").is_none());
    }

    #[test]
    fn parses_modern_three_column_search_rows() {
        let stdout = "\
Looking for xz in package list. Please wait... DONE

The list below shows all packages with name matching \"xz\".

[ Status           ] [ Repository               ] [ Package                                  ]
[   installed      ] [ slackware64              ] [ xz-5.4.4-x86_64-1                        ]
[ uninstalled      ] [ slackware64              ] [ xzgv-0.9.2-x86_64-2                      ]
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "xz");
        assert_eq!(packages[0].version.as_deref(), Some("5.4.4"));
        assert_eq!(packages[0].origin.as_deref(), Some("slackware64"));
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[1].state, InstallState::Available);
    }

    #[test]
    fn parses_old_single_bracket_search_rows() {
        let stdout = "\
[ installed ] - xz-5.2.2-x86_64-1
[uninstalled] - xzgv-0.9.1-x86_64-1
[  upgrade  ] - bash-5.2.021-x86_64-1
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "xz");
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[2].name, "bash");
        assert_eq!(packages[2].state, InstallState::Upgradable);
        assert_eq!(packages[2].version, None);
        assert_eq!(packages[2].latest_version.as_deref(), Some("5.2.021"));
    }

    #[test]
    fn parses_info_stanzas() {
        let stdout = "\
PACKAGE NAME:  xz-5.4.4-x86_64-1.txz
PACKAGE LOCATION:  ./slackware64/a
PACKAGE SIZE (compressed):  461 K
PACKAGE SIZE (uncompressed):  1660 K
PACKAGE DESCRIPTION:
xz: xz (compression utility based on the LZMA algorithm)
xz:
xz: xz provides very high compression ratios.
";
        let packages = parse_info(stdout);
        assert_eq!(packages.len(), 1);
        let package = &packages[0];
        assert_eq!(package.name, "xz");
        assert_eq!(package.version.as_deref(), Some("5.4.4"));
        assert_eq!(package.architecture.as_deref(), Some("x86_64"));
        assert_eq!(package.origin.as_deref(), Some("slackware64/a"));
        assert_eq!(package.download_size, Some(461 * 1024));
        assert_eq!(package.installed_size, Some(1660 * 1024));
        assert_eq!(
            package.description.as_deref(),
            Some("xz (compression utility based on the LZMA algorithm)")
        );
    }

    #[test]
    fn parses_multiple_info_stanzas() {
        let stdout = "\
PACKAGE NAME:  xz-5.4.4-x86_64-1.txz
PACKAGE LOCATION:  ./slackware64/a
PACKAGE DESCRIPTION:
xz: xz (compression utility)

PACKAGE NAME:  xzgv-0.9.2-x86_64-2.txz
PACKAGE LOCATION:  ./slackware64/xap
PACKAGE DESCRIPTION:
xzgv: xzgv (picture viewer)
";
        let packages = parse_info(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "xzgv");
        assert_eq!(packages[1].description.as_deref(), Some("xzgv (picture viewer)"));
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("461 K"), Some(461 * 1024));
        assert_eq!(parse_size("1.6M"), Some((1.6 * 1024.0 * 1024.0) as u64));
        assert_eq!(parse_size("garbage"), None);
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("xz@5.4.4")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("xz")]).is_ok());
    }
}
