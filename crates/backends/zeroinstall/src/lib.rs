//! Zero Install backend for snowcone.
//!
//! 0install's unit is the feed URI. The Linux (OCaml) client manages
//! per-user "apps": pet names bound to a feed with `0install add NAME URI`
//! and removed with `0install destroy NAME`. Install therefore requires
//! request names to be feed URIs (the pet name is derived from the URI's
//! last path segment), while installed packages go by their pet names.
//! The OCaml client has no `list-apps`/`update-apps` verbs (those belong
//! to the Windows client), so the app list is read from the same
//! `0install.net/apps` config directories 0install itself scans, and
//! upgrade-all iterates `0install update` over it. Every invocation
//! passes `--console` so the GTK GUI never pops up; `--dry-run` is a
//! native option on every verb. Per-user throughout - nothing elevates.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "0install";
const PROGRAMS: &[&str] = &["0install"];

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
    fn cmd(&self, subcommand: &str) -> Cmd {
        Cmd::new(&self.program).arg(subcommand).arg("--console")
    }

    /// Read invocation with a stable locale, so parsing survives i18n.
    fn query(&self, subcommand: &str) -> Cmd {
        self.cmd(subcommand).env("LC_ALL", "C")
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

    /// One installed app, named by its pet name; a failed solve still
    /// names the app, so `show` errors degrade to a bare entry.
    async fn app_details(&self, name: &str) -> Result<ZeroInstallPackage> {
        let mut package = ZeroInstallPackage {
            name: name.to_string(),
            state: InstallState::Installed,
            ..Default::default()
        };
        let output = self
            .query("show")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if output.success()
            && let Some((uri, version)) = parse_selections(&output.stdout)
        {
            package.origin = Some(uri);
            package.version = version;
        }
        Ok(package)
    }
}

/// 0install resolves each feed to its own selected version; nothing pins.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but this backend installs each feed's selected version"
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
        "Zero Install"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Universal
    }

    fn database_id(&self) -> &'static str {
        "0install"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::SEARCH | Capabilities::UPGRADE
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        for package in packages {
            let name = pet_name(&package.name).ok_or_else(|| {
                Error::Other(format!(
                    "{ID}: `{}` is not a feed URI - 0install installs from URIs like \
                     https://apps.0install.net/utils/hello.xml (try search)",
                    package.name
                ))
            })?;
            let mut cmd = self.cmd("add");
            if ctx.dry_run {
                cmd = cmd.arg("--dry-run");
            }
            self.run(cmd.arg(name).arg(&package.name), ctx).await?;
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        for package in packages {
            let mut cmd = self.cmd("destroy");
            if ctx.dry_run {
                cmd = cmd.arg("--dry-run");
            }
            self.run(cmd.arg(&package.name), ctx).await?;
        }
        Ok(())
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let mut packages = Vec::new();
        for name in installed_app_names() {
            packages.push(Box::new(self.app_details(&name).await?) as Box<dyn Package>);
        }
        Ok(packages)
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        if installed_app_names().iter().any(|app| app == name) {
            return Ok(Box::new(self.app_details(name).await?));
        }
        // Not an app: a feed URI can still be solved and described.
        if !name.contains("://") {
            return Err(Error::NotFound(name.to_string()));
        }
        let output = self
            .query("select")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        let (uri, version) = parse_selections(&output.stdout).ok_or_else(|| Error::Parse {
            what: format!("{ID} select output"),
            detail: format!("no `URI` line for `{name}`"),
        })?;
        Ok(Box::new(ZeroInstallPackage {
            name: uri,
            version,
            state: InstallState::Available,
            ..Default::default()
        }))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query("search")
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_search(&output.stdout)
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        let names: Vec<String> = if packages.is_empty() {
            installed_app_names()
        } else {
            packages.iter().map(|package| package.name.clone()).collect()
        };
        for name in names {
            let mut cmd = self.cmd("update");
            if ctx.dry_run {
                cmd = cmd.arg("--dry-run");
            }
            self.run(cmd.arg(&name), ctx).await?;
        }
        Ok(())
    }
}

/// Config directories the OCaml client scans for apps, in precedence
/// order: `$ZEROINSTALL_PORTABLE_BASE/config`, otherwise `$XDG_CONFIG_HOME`
/// (default `~/.config`) then every `$XDG_CONFIG_DIRS` entry (default
/// `/etc/xdg`), each suffixed `0install.net/apps` in the XDG case.
fn app_dirs() -> Vec<PathBuf> {
    if let Some(base) = std::env::var_os("ZEROINSTALL_PORTABLE_BASE") {
        return vec![PathBuf::from(base).join("config").join("apps")];
    }
    let mut dirs = Vec::new();
    match std::env::var_os("XDG_CONFIG_HOME").filter(|home| !home.is_empty()) {
        Some(home) => dirs.push(PathBuf::from(home)),
        None => {
            if let Some(home) = std::env::var_os("HOME") {
                dirs.push(PathBuf::from(home).join(".config"));
            }
        }
    }
    let config_dirs = std::env::var("XDG_CONFIG_DIRS")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "/etc/xdg".to_string());
    dirs.extend(config_dirs.split(':').filter(|dir| !dir.is_empty()).map(PathBuf::from));
    dirs.into_iter()
        .map(|dir| dir.join("0install.net").join("apps"))
        .collect()
}

/// Pet names of the installed apps: directory entries matching the
/// client's own app-name rule, first directory wins on duplicates.
fn installed_app_names() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for dir in app_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str()
                && valid_app_name(name)
                && !names.iter().any(|existing| existing == name)
            {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    names
}

/// The client's app-name rule: not empty, not starting with a dot, and
/// free of path separators and shell-hostile characters.
fn valid_app_name(name: &str) -> bool {
    const FORBIDDEN: [char; 7] = ['/', '\\', ':', '=', ';', '\'', '"'];
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first != '.' && !FORBIDDEN.contains(&first) && !chars.any(|c| FORBIDDEN.contains(&c))
}

/// Derive an app pet name from a feed URI: the last path segment with any
/// `.xml` suffix stripped (`…/hello.xml` becomes `hello`).
fn pet_name(uri: &str) -> Option<String> {
    if !uri.contains("://") {
        return None;
    }
    let tail = uri.trim_end_matches('/').rsplit('/').next()?;
    let name = tail.strip_suffix(".xml").unwrap_or(tail);
    valid_app_name(name).then(|| name.to_string())
}

/// `0install show`/`select` selection trees: the root node's `- URI:` and
/// `Version:` lines; a failed solve prints `No selected version` instead
/// of a Version line, and `(requires compilation)` may trail the version.
fn parse_selections(stdout: &str) -> Option<(String, Option<String>)> {
    let uri = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("- URI:"))?
        .trim()
        .to_string();
    let version = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Version:"))
        .and_then(|value| value.split_whitespace().next())
        .map(str::to_string);
    Some((uri, version))
}

/// `0install search`: a feed URI on its own line, then an indented
/// `Name - Summary [score%]` detail line, with a blank line between
/// results; the URI is the package name because it is what install takes.
fn parse_search(stdout: &str) -> Vec<ZeroInstallPackage> {
    let mut packages: Vec<ZeroInstallPackage> = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with(char::is_whitespace) {
            if let Some(last) = packages.last_mut()
                && last.description.is_none()
            {
                let mut detail = line.trim();
                if let Some(score) = detail.rfind(" [")
                    && detail.ends_with("%]")
                {
                    detail = detail[..score].trim_end();
                }
                if !detail.is_empty() {
                    last.description = Some(detail.to_string());
                }
            }
            continue;
        }
        packages.push(ZeroInstallPackage {
            name: line.trim().to_string(),
            state: InstallState::Available,
            ..Default::default()
        });
    }
    packages
}

/// A package as 0install describes it.
#[derive(Debug, Default)]
pub struct ZeroInstallPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub origin: Option<String>,
    pub state: InstallState,
}

impl Package for ZeroInstallPackage {
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
    fn parses_selection_trees() {
        let stdout = "\
- URI: https://apps.0install.net/utils/hello.xml
  Version: 1.3
  Path: /home/nick/.cache/0install.net/implementations/sha256new_abc

  - URI: https://apps.0install.net/lib/glibc.xml
    Version: 2.39
    Path: (package:glibc)
";
        let (uri, version) = parse_selections(stdout).unwrap();
        assert_eq!(uri, "https://apps.0install.net/utils/hello.xml");
        assert_eq!(version.as_deref(), Some("1.3"));
    }

    #[test]
    fn selection_version_drops_compilation_marker() {
        let stdout = "\
- URI: https://example.com/tool.xml
  Version: 0.9 (requires compilation)
  Path: (not cached)
";
        let (_, version) = parse_selections(stdout).unwrap();
        assert_eq!(version.as_deref(), Some("0.9"));
    }

    #[test]
    fn unsolved_selection_has_no_version() {
        let stdout = "\
- URI: https://example.com/tool.xml
  No selected version
";
        let (uri, version) = parse_selections(stdout).unwrap();
        assert_eq!(uri, "https://example.com/tool.xml");
        assert_eq!(version, None);
    }

    #[test]
    fn parses_search_results() {
        let stdout = "\
https://apps.0install.net/utils/edit.xml
  Edit - A simple text editor [85%]

https://example.com/hello.xml
  Hello - Friendly greeting program [42%]
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "https://apps.0install.net/utils/edit.xml");
        assert_eq!(
            packages[0].description.as_deref(),
            Some("Edit - A simple text editor")
        );
        assert_eq!(packages[1].name, "https://example.com/hello.xml");
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn derives_pet_names_from_feed_uris() {
        assert_eq!(
            pet_name("https://apps.0install.net/utils/hello.xml").as_deref(),
            Some("hello")
        );
        assert_eq!(
            pet_name("https://example.com/tool").as_deref(),
            Some("tool")
        );
        assert_eq!(pet_name("hello"), None);
    }

    #[test]
    fn validates_app_names() {
        assert!(valid_app_name("hello"));
        assert!(valid_app_name("hello-world_2"));
        assert!(!valid_app_name(".hidden"));
        assert!(!valid_app_name("a:b"));
        assert!(!valid_app_name(""));
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("hello@1.3")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("hello")]).is_ok());
    }
}
