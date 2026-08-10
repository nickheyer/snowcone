//! XBPS backend for snowcone.
//!
//! Void's package manager is a suite of binaries over one database:
//! `xbps-install` mutates, `xbps-remove` removes, `xbps-query` reads.
//! Discovery keys on `xbps-install`; the siblings are resolved at
//! construction. install/remove/upgrade take a native `-n` dry-run, and
//! the outdated listing reuses it (`xbps-install -un` against the cached
//! repodata, so no root and no sync). Mutations run through the elevation
//! helper with `-y` for `assume_yes`. pkgver strings are
//! `name-version_revision`, split at the last dash.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "xbps";
const PROGRAMS: &[&str] = &["xbps-install"];

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
        let sibling =
            |name: &str| find_program(name).ok_or_else(|| Error::Unavailable(ID.to_string()));
        Ok(Box::new(Manager {
            xbps_install: sibling("xbps-install")?,
            xbps_remove: sibling("xbps-remove")?,
            xbps_query: sibling("xbps-query")?,
            elevator: Elevator::detect(host),
        }))
    }
}

struct Manager {
    xbps_install: PathBuf,
    xbps_remove: PathBuf,
    xbps_query: PathBuf,
    elevator: Elevator,
}

impl Manager {
    /// Mutating `xbps-install` invocation, in the user's locale, elevated,
    /// with the yes and native dry-run switches.
    fn installer(&self, ctx: &OpContext) -> Cmd {
        let mut cmd = Cmd::new(&self.xbps_install).elevated(true);
        if ctx.assume_yes {
            cmd = cmd.arg("-y");
        }
        if ctx.dry_run {
            cmd = cmd.arg("-n");
        }
        cmd
    }

    /// Read invocation of `xbps-query` with a stable locale, so parsing
    /// survives i18n.
    fn query(&self) -> Cmd {
        Cmd::new(&self.xbps_query).env("LC_ALL", "C")
    }

    /// Read invocation of `xbps-install` (dry-run previews) with a stable
    /// locale.
    fn preview(&self) -> Cmd {
        Cmd::new(&self.xbps_install).env("LC_ALL", "C")
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

/// xbps has no version selection: installs always take the repository head.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but xbps only installs the repository head"
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
        "XBPS"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "xbps"
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
        let cmd = self
            .installer(ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let mut cmd = Cmd::new(&self.xbps_remove).elevated(true);
        if ctx.assume_yes {
            cmd = cmd.arg("-y");
        }
        if ctx.dry_run {
            cmd = cmd.arg("-n");
        }
        cmd = cmd.args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("-l")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_list(&output.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // Plain `-S` reads the local pkgdb, so a hit means installed;
        // `-R` extends the lookup to the repositories.
        let installed = self
            .query()
            .arg("-S")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        let (output, state) = if installed.success() && !installed.stdout.trim().is_empty() {
            (installed, InstallState::Installed)
        } else {
            let available = self
                .query()
                .args(["-R", "-S"])
                .arg(name)
                .capture(&self.elevator, None)
                .await?;
            if !available.success() || available.stdout.trim().is_empty() {
                return Err(Error::NotFound(name.to_string()));
            }
            (available, InstallState::Available)
        };
        match parse_show(&output.stdout, state) {
            Some(package) => Ok(Box::new(package)),
            None => Err(Error::Parse {
                what: format!("{ID} show output"),
                detail: format!("no `pkgname` field for `{name}`"),
            }),
        }
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("-Rs")
            .arg(query)
            .capture(&self.elevator, None)
            .await?;
        // Exits non-zero with no output when nothing matches.
        if !output.success() && output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(boxed(parse_search(&output.stdout)))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: refresh has no dry-run mode")));
        }
        self.run(Cmd::new(&self.xbps_install).arg("-S").elevated(true), ctx)
            .await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        let cmd = if packages.is_empty() {
            // `-Su` is Void's documented full upgrade: sync, then update.
            self.installer(ctx).arg("-Su")
        } else {
            self.installer(ctx)
                .arg("-u")
                .args(packages.iter().map(|package| package.name.as_str()))
        };
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .preview()
            .arg("-un")
            .capture(&self.elevator, None)
            .await?;
        // Exits non-zero with no transaction output when everything is
        // current.
        if !output.success() && output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(boxed(parse_outdated(&output.stdout)))
    }
}

fn boxed(packages: Vec<XbpsPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `xbps-triggers-0.124_1` → `("xbps-triggers", Some("0.124_1"))`: xbps
/// pkgvers put the version after the last dash; requiring a digit after it
/// also sheds non-package lines.
fn split_pkgver(pkgver: &str) -> (String, Option<String>) {
    for (idx, _) in pkgver.match_indices('-').rev() {
        if pkgver[idx + 1..].starts_with(|c: char| c.is_ascii_digit()) {
            return (
                pkgver[..idx].to_string(),
                Some(pkgver[idx + 1..].to_string()),
            );
        }
    }
    (pkgver.to_string(), None)
}

/// `glibc>=2.36_1` or `libvlc-3.0.20_1` → the bare dependency name.
fn dep_name(entry: &str) -> String {
    match entry.find(['<', '>', '=']) {
        Some(idx) => entry[..idx].to_string(),
        None => split_pkgver(entry).0,
    }
}

/// `xbps-query -l`: `ii pkgname-1.2_1 short description` rows, the first
/// token being the two pkgdb state flag characters.
fn parse_list(stdout: &str) -> Vec<XbpsPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            if parts.next()?.len() != 2 {
                return None;
            }
            let (name, version) = split_pkgver(parts.next()?);
            version.as_ref()?;
            let description = parts.collect::<Vec<_>>().join(" ");
            Some(XbpsPackage {
                name,
                version,
                description: (!description.is_empty()).then_some(description),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `xbps-query -Rs`: `[*] pkgname-1.2_1 short description` rows, `[*]`
/// marking installed and `[-]` not.
fn parse_search(stdout: &str) -> Vec<XbpsPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let state = match parts.next()? {
                "[*]" => InstallState::Installed,
                "[-]" => InstallState::Available,
                _ => return None,
            };
            let (name, version) = split_pkgver(parts.next()?);
            version.as_ref()?;
            let description = parts.collect::<Vec<_>>().join(" ");
            Some(XbpsPackage {
                name,
                version,
                description: (!description.is_empty()).then_some(description),
                state,
                ..Default::default()
            })
        })
        .collect()
}

/// `xbps-query -S`/`-RS`: `key: value` properties; list-valued keys
/// (`run_depends`) print the key alone with one indented entry per line.
fn parse_show(stdout: &str, state: InstallState) -> Option<XbpsPackage> {
    let mut package = XbpsPackage {
        state,
        ..Default::default()
    };
    let mut in_run_depends = false;
    for line in stdout.lines() {
        if line.starts_with(char::is_whitespace) {
            let entry = line.trim();
            if in_run_depends && !entry.is_empty() {
                package
                    .dependencies
                    .get_or_insert_with(Vec::new)
                    .push(dep_name(entry));
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        in_run_depends = key == "run_depends";
        if value.is_empty() {
            continue;
        }
        match key {
            "pkgname" => package.name = value.to_string(),
            "pkgver" => {
                let (name, version) = split_pkgver(value);
                if package.name.is_empty() {
                    package.name = name;
                }
                package.version = version;
            }
            "short_desc" => package.description = Some(value.to_string()),
            "homepage" => package.homepage = Some(value.to_string()),
            "license" => package.license = Some(value.to_string()),
            "architecture" => package.architecture = Some(value.to_string()),
            "repository" => package.origin = Some(value.to_string()),
            _ => {}
        }
    }
    (!package.name.is_empty()).then_some(package)
}

/// `xbps-install -un`: `pkgver action arch repository …` transaction rows;
/// only `update` rows are outdated packages (new dependencies appear as
/// `install`), and the row names just the incoming version.
fn parse_outdated(stdout: &str) -> Vec<XbpsPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pkgver = parts.next()?;
            if parts.next()? != "update" {
                return None;
            }
            let (name, version) = split_pkgver(pkgver);
            let latest = version?;
            let _arch = parts.next();
            Some(XbpsPackage {
                name,
                latest_version: Some(latest),
                origin: parts.next().map(str::to_string),
                state: InstallState::Upgradable,
                ..Default::default()
            })
        })
        .collect()
}

/// A package as xbps describes it.
#[derive(Debug, Default)]
pub struct XbpsPackage {
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

impl Package for XbpsPackage {
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
    fn splits_pkgver_strings() {
        assert_eq!(
            split_pkgver("xbps-triggers-0.124_1"),
            ("xbps-triggers".to_string(), Some("0.124_1".to_string()))
        );
        assert_eq!(
            split_pkgver("firefox-121.0_1"),
            ("firefox".to_string(), Some("121.0_1".to_string()))
        );
        assert_eq!(split_pkgver("firefox"), ("firefox".to_string(), None));
    }

    #[test]
    fn parses_installed_list_rows() {
        let stdout = "\
ii bash-5.2.26_1                    GNU Bourne Again Shell
ii xbps-triggers-0.124_1            XBPS triggers for xbps-src
";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "xbps-triggers");
        assert_eq!(packages[1].version.as_deref(), Some("0.124_1"));
        assert_eq!(
            packages[1].description.as_deref(),
            Some("XBPS triggers for xbps-src")
        );
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn parses_search_rows() {
        let stdout = "\
[*] zsh-5.9_4            Z SHell
[-] zsh-autosuggestions-0.7.0_1 Fish-like autosuggestions for zsh
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "zsh");
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[1].name, "zsh-autosuggestions");
        assert_eq!(packages[1].version.as_deref(), Some("0.7.0_1"));
        assert_eq!(packages[1].state, InstallState::Available);
    }

    #[test]
    fn parses_show_properties() {
        let stdout = "\
architecture: x86_64
homepage: https://www.zsh.org
installed_size: 6MB
license: MIT
pkgname: zsh
pkgver: zsh-5.9_4
repository: https://repo-default.voidlinux.org/current
run_depends:
\tncurses>=5.8_1
\tglibc>=2.36_1
short_desc: Z SHell
";
        let package = parse_show(stdout, InstallState::Installed).unwrap();
        assert_eq!(package.name, "zsh");
        assert_eq!(package.version.as_deref(), Some("5.9_4"));
        assert_eq!(package.description.as_deref(), Some("Z SHell"));
        assert_eq!(package.homepage.as_deref(), Some("https://www.zsh.org"));
        assert_eq!(package.license.as_deref(), Some("MIT"));
        assert_eq!(package.architecture.as_deref(), Some("x86_64"));
        assert_eq!(
            package.origin.as_deref(),
            Some("https://repo-default.voidlinux.org/current")
        );
        assert_eq!(
            package.dependencies,
            Some(vec!["ncurses".to_string(), "glibc".to_string()])
        );
        assert_eq!(package.state, InstallState::Installed);
    }

    #[test]
    fn parses_dry_run_update_rows() {
        let stdout = "\
firefox-121.0_1 update x86_64 https://repo-default.voidlinux.org/current 56340339 55411308
nss-3.96_1 install x86_64 https://repo-default.voidlinux.org/current 1943040 5636096
";
        let packages = parse_outdated(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "firefox");
        assert_eq!(packages[0].latest_version.as_deref(), Some("121.0_1"));
        assert_eq!(
            packages[0].origin.as_deref(),
            Some("https://repo-default.voidlinux.org/current")
        );
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("zsh@5.9_4")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("zsh")]).is_ok());
    }
}
