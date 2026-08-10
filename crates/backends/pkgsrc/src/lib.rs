//! pkgsrc backend for snowcone.
//!
//! Drives `pkgin`, pkgsrc's binary package client. pkgin takes its global
//! flags before the command word; `-n` answers "no" while printing the
//! planned actions, which gives install/remove/upgrade a native dry-run
//! (`update` has none). Search results carry `=`/`<`/`>` install-state
//! markers plus a trailing legend that must be skipped when parsing, and
//! pkgin has no outdated-listing verb - list-outdated filters the `<`
//! (newer version available) markers out of a whole-repository search and
//! joins the installed listing for current versions. The default pkgsrc
//! prefix is root-owned, so mutations run through the elevation helper.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "pkgsrc";
const PROGRAMS: &[&str] = &["pkgin"];

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

    /// Shared shape for mutating commands: elevated, with `-n` (preview,
    /// answer "no") or `-y` placed before the subcommand as pkgin requires.
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().elevated(true);
        if ctx.dry_run {
            cmd = cmd.arg("-n");
        } else if ctx.assume_yes {
            cmd = cmd.arg("-y");
        }
        cmd.arg(subcommand)
    }

    /// Installed packages, from `pkgin list`.
    async fn installed(&self) -> Result<Vec<PkgsrcPackage>> {
        let output = self
            .query()
            .arg("list")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_listing(&output.stdout, InstallState::Installed))
    }

    /// `pkgin search`, mapping the non-zero "no results" exit to an empty
    /// set (pkgin prints matches, when any, on stdout).
    async fn search_repo(&self, pattern: &str) -> Result<Vec<PkgsrcPackage>> {
        let output = self
            .query()
            .arg("search")
            .arg(pattern)
            .capture(&self.elevator, None)
            .await?;
        if output.success() || !output.stdout.trim().is_empty() {
            Ok(parse_search(&output.stdout))
        } else {
            Ok(Vec::new())
        }
    }
}

/// Installs always take the repository's current binary package.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but pkgin installs the repository's current package"
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
        "pkgsrc"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Universal
    }

    fn database_id(&self) -> &'static str {
        "pkgsrc"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::REFRESH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
    }

    fn needs_elevation(&self, operation: Operation) -> bool {
        operation.mutates()
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        let cmd = self
            .mutation("install", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = self
            .mutation("remove", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.installed().await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // `avail` may fail before the first `pkgin update`; the installed
        // side still answers then.
        let avail = self
            .query()
            .arg("avail")
            .capture(&self.elevator, None)
            .await?;
        let candidate = parse_listing(&avail.stdout, InstallState::Available)
            .into_iter()
            .find(|package| is_named(package, name));
        let installed = self
            .installed()
            .await?
            .into_iter()
            .find(|package| is_named(package, name));
        let package = match (installed, candidate) {
            (Some(mut installed), Some(candidate)) => {
                if installed.description.is_none() {
                    installed.description = candidate.description;
                }
                if candidate.version != installed.version {
                    installed.latest_version = candidate.version;
                    installed.state = InstallState::Upgradable;
                }
                installed
            }
            (Some(installed), None) => installed,
            (None, Some(candidate)) => candidate,
            (None, None) => return Err(Error::NotFound(name.to_string())),
        };
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.search_repo(query).await?))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: refresh has no dry-run mode")));
        }
        self.run(self.cmd().arg("update").elevated(true), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        let cmd = if packages.is_empty() {
            self.mutation("full-upgrade", ctx)
        } else {
            // pkgin has no targeted upgrade verb; `install` brings an
            // already-installed package to the newest candidate.
            self.mutation("install", ctx)
                .args(packages.iter().map(|package| package.name.as_str()))
        };
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        // No native verb: a whole-repository search marks installed-but-older
        // packages with `<`; the installed listing supplies current versions.
        let search = self.search_repo(".").await?;
        let installed = self.installed().await?;
        Ok(boxed(outdated_from(search, installed)))
    }
}

fn boxed(packages: Vec<PkgsrcPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// pkgsrc `PKGBASE-PKGVERSION` names: the version is everything after the
/// last dash and by convention starts with a digit (`vim-9.0.2136`,
/// `unzip-6.0nb10`); names without such a suffix carry no version.
fn split_pkg(full: &str) -> (String, Option<String>) {
    if let Some((name, version)) = full.rsplit_once('-')
        && version.starts_with(|c: char| c.is_ascii_digit())
    {
        return (name.to_string(), Some(version.to_string()));
    }
    (full.to_string(), None)
}

/// Match by pkgbase (`vim`) or by the exact `pkgbase-version` string.
fn is_named(package: &PkgsrcPackage, name: &str) -> bool {
    package.name == name
        || package
            .version
            .as_deref()
            .and_then(|version| name.strip_suffix(version))
            .and_then(|prefix| prefix.strip_suffix('-'))
            == Some(package.name.as_str())
}

/// `pkgin list`/`pkgin avail`: `name-version  comment` lines; anything whose
/// first token lacks a version suffix ("No results" chatter, warnings) is
/// skipped.
fn parse_listing(stdout: &str, state: InstallState) -> Vec<PkgsrcPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let (name, version) = split_pkg(parts.next()?);
            let version = version?;
            let comment = parts.collect::<Vec<_>>().join(" ");
            Some(PkgsrcPackage {
                name,
                version: Some(version),
                description: (!comment.is_empty()).then_some(comment),
                state,
                ..Default::default()
            })
        })
        .collect()
}

/// `pkgin search`: `name-version [marker] comment` lines - `=`/`>` mark an
/// installed result, `<` an upgradable one - followed by a legend whose
/// lines start with those marker characters and are skipped, as is any
/// versionless chatter.
fn parse_search(stdout: &str) -> Vec<PkgsrcPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            if line.starts_with(['=', '<', '>']) {
                return None;
            }
            let mut parts = line.split_whitespace().peekable();
            let (name, version) = split_pkg(parts.next()?);
            let version = version?;
            let state = match parts.peek() {
                Some(&"=") | Some(&">") => {
                    parts.next();
                    InstallState::Installed
                }
                Some(&"<") => {
                    parts.next();
                    InstallState::Upgradable
                }
                _ => InstallState::Available,
            };
            let comment = parts.collect::<Vec<_>>().join(" ");
            Some(PkgsrcPackage {
                name,
                version: Some(version),
                description: (!comment.is_empty()).then_some(comment),
                state,
                ..Default::default()
            })
        })
        .collect()
}

/// Search hits flagged `<` (the line names the candidate version), re-keyed
/// so `version` is the installed one and `latest_version` the candidate.
fn outdated_from(search: Vec<PkgsrcPackage>, installed: Vec<PkgsrcPackage>) -> Vec<PkgsrcPackage> {
    search
        .into_iter()
        .filter(|package| package.state == InstallState::Upgradable)
        .map(|mut package| {
            package.latest_version = package.version.take();
            package.version = installed
                .iter()
                .find(|current| current.name == package.name)
                .and_then(|current| current.version.clone());
            package
        })
        .collect()
}

/// A package as pkgin describes it.
#[derive(Debug, Default)]
pub struct PkgsrcPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for PkgsrcPackage {
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

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_pkgsrc_names() {
        assert_eq!(
            split_pkg("unzip-6.0nb10"),
            ("unzip".to_string(), Some("6.0nb10".to_string()))
        );
        assert_eq!(
            split_pkg("python311-3.11.5"),
            ("python311".to_string(), Some("3.11.5".to_string()))
        );
        assert_eq!(
            split_pkg("xf86-video-vesa-2.6.0"),
            ("xf86-video-vesa".to_string(), Some("2.6.0".to_string()))
        );
        assert_eq!(split_pkg("go-tools"), ("go-tools".to_string(), None));
    }

    #[test]
    fn parses_installed_listing() {
        let stdout = "\
pkgin-23.8.1nb1      Package manager using a binary repository
vim-9.0.2136         Vim editor (vi clone) without GUI
";
        let packages = parse_listing(stdout, InstallState::Installed);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "vim");
        assert_eq!(packages[1].version.as_deref(), Some("9.0.2136"));
        assert_eq!(
            packages[1].description.as_deref(),
            Some("Vim editor (vi clone) without GUI")
        );
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn parses_search_markers_and_skips_the_legend() {
        let stdout = "\
ripgrep-14.1.0 <     Line oriented search tool using Rust's regex library
tmux-3.4 =           Terminal multiplexer
mosh-1.4.0 >         Remote terminal application
fd-9.0.0             Simple, fast alternative to find

=: package is installed and up-to-date
<: package is installed but newer version is available
>: installed package has a greater version than available package
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 4);
        assert_eq!(packages[0].name, "ripgrep");
        assert_eq!(packages[0].state, InstallState::Upgradable);
        assert_eq!(packages[1].state, InstallState::Installed);
        assert_eq!(packages[2].state, InstallState::Installed);
        assert_eq!(packages[3].state, InstallState::Available);
        assert_eq!(
            packages[3].description.as_deref(),
            Some("Simple, fast alternative to find")
        );
    }

    #[test]
    fn joins_outdated_with_installed_versions() {
        let search = parse_search("ripgrep-14.1.0 <  Line oriented search tool\n");
        let installed = parse_listing("ripgrep-13.0.0  Line oriented search tool\n", InstallState::Installed);
        let outdated = outdated_from(search, installed);
        assert_eq!(outdated.len(), 1);
        assert_eq!(outdated[0].version.as_deref(), Some("13.0.0"));
        assert_eq!(outdated[0].latest_version.as_deref(), Some("14.1.0"));
        assert_eq!(outdated[0].state, InstallState::Upgradable);
    }

    #[test]
    fn matches_pkgbase_and_full_names() {
        let package = PkgsrcPackage {
            name: "vim".to_string(),
            version: Some("9.0.2136".to_string()),
            ..Default::default()
        };
        assert!(is_named(&package, "vim"));
        assert!(is_named(&package, "vim-9.0.2136"));
        assert!(!is_named(&package, "vim-share"));
        assert!(!is_named(&package, "neovim"));
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("ripgrep@14.1.0")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("ripgrep")]).is_ok());
    }
}
