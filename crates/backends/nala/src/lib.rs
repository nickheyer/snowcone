//! Nala backend for snowcone.
//!
//! Drives nala, the friendlier apt frontend over the same dpkg database.
//! The shipped nala (the Python 0.14/0.15 line) has no simulate flag at
//! all, so every mutation errors under `--dry-run`. Queries pass
//! `--no-color` (nala colors output even when piped) and run under
//! `LC_ALL=C`, where nala swaps its tree glyphs for ASCII - the parsers
//! accept both `├──`/`└──` and `+--`/`` `-- ``. `nala upgrade` accepts no
//! package arguments, so targeted upgrades go through `install`, which
//! brings installed packages to their candidate version.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "nala";
const PROGRAMS: &[&str] = &["nala"];

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
    /// Call sites add `--no-color` after the subcommand - nala colors its
    /// output even when piped.
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

    /// Shared flags for mutating commands: elevation and `-y`.
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(subcommand).elevated(true);
        if ctx.assume_yes {
            cmd = cmd.arg("-y");
        }
        cmd
    }

    /// `nala list` variant; nala exits non-zero with an empty stdout when
    /// nothing matches, which is an empty result, not a failure.
    async fn list_query(&self, flag: &str, pattern: Option<&str>) -> Result<Vec<NalaPackage>> {
        let mut cmd = self.query().args(["list", "--no-color", flag]);
        if let Some(pattern) = pattern {
            cmd = cmd.arg(pattern);
        }
        let output = cmd.capture(&self.elevator, None).await?;
        if !output.success() && output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(parse_list(&output.stdout))
    }
}

/// `name=version` when the request pins one, bare name otherwise.
fn spec(request: &PackageRequest) -> String {
    match &request.version {
        Some(version) => format!("{}={version}", request.name),
        None => request.name.clone(),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Nala"
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
            | Capabilities::REFRESH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
            | Capabilities::PIN_VERSION
    }

    fn needs_elevation(&self, operation: Operation) -> bool {
        matches!(
            operation,
            Operation::Install | Operation::Remove | Operation::Upgrade | Operation::Refresh
        )
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        let cmd = self
            .mutation("install", ctx)
            .args(packages.iter().map(spec));
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

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.list_query("--installed", None).await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let show = self
            .query()
            .args(["show", "--no-color"])
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !show.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        let mut package = parse_show(&show.stdout).ok_or_else(|| Error::NotFound(name.to_string()))?;
        // `Installed: yes` only covers the shown candidate; one list probe
        // fills in the actually-installed version and upgradability.
        if let Some(listed) = self
            .list_query("--installed", Some(&package.name))
            .await?
            .into_iter()
            .find(|listed| listed.name == package.name)
        {
            package.state = listed.state;
            package.version = listed.version;
            if listed.latest_version.is_some() {
                package.latest_version = listed.latest_version;
            }
        }
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["search", "--no-color"])
            .arg(query)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() && output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(boxed(parse_list(&output.stdout)))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("refresh"));
        }
        self.run(self.cmd().arg("update").elevated(true), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        let cmd = if packages.is_empty() {
            self.mutation("upgrade", ctx)
        } else {
            // `nala upgrade` takes no package arguments; `install` brings
            // an installed package to its candidate version.
            self.mutation("install", ctx).args(packages.iter().map(spec))
        };
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.list_query("--upgradable", None).await?))
    }
}

fn boxed(packages: Vec<NalaPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// Tree-line prefixes: (status, description) pairs; nala prints the UTF-8
/// forms in UTF-8 locales and the ASCII forms under `LC_ALL=C`.
const STATUS_PREFIXES: [&str; 2] = ["├──", "+--"];
const DESCRIPTION_PREFIXES: [&str; 2] = ["└──", "`--"];

fn strip_any<'a>(line: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes
        .iter()
        .find_map(|prefix| line.strip_prefix(prefix))
        .map(str::trim)
}

/// `nala list`/`nala search` blocks: a `name version [origin]` header, then
/// an optional `├── is installed …` status line and a `└── description`
/// line. Upgradable entries carry the other version in the status line,
/// on either side depending on which version heads the block.
fn parse_list(stdout: &str) -> Vec<NalaPackage> {
    let mut packages: Vec<NalaPackage> = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(status) = strip_any(line, &STATUS_PREFIXES) {
            let Some(last) = packages.last_mut() else {
                continue;
            };
            if let Some(rest) = status.strip_prefix("is installed and upgradable to ") {
                last.state = InstallState::Upgradable;
                last.latest_version = Some(rest.trim().to_string());
            } else if let Some(rest) = status.strip_prefix("is upgradable from ") {
                last.state = InstallState::Upgradable;
                last.latest_version = last.version.take();
                last.version = Some(rest.trim().to_string());
            } else if status.starts_with("is installed") {
                last.state = InstallState::Installed;
            }
            continue;
        }
        if let Some(description) = strip_any(line, &DESCRIPTION_PREFIXES) {
            if let Some(last) = packages.last_mut()
                && last.description.is_none()
                && !description.is_empty()
            {
                last.description = Some(description.to_string());
            }
            continue;
        }
        if line.starts_with(char::is_whitespace) || line.starts_with('│') || line.starts_with('|') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(name), Some(version)) = (parts.next(), parts.next()) else {
            continue;
        };
        packages.push(NalaPackage {
            name: name.to_string(),
            version: Some(version.to_string()),
            origin: line
                .split_once('[')
                .map(|(_, rest)| rest.trim_end().trim_end_matches(']').to_string()),
            state: InstallState::Available,
            ..Default::default()
        });
    }
    packages
}

/// `nala show`: `Key: Value` fields. `Depends:` may spill onto indented
/// follow-up lines; `Description:` is always last and swallows the rest of
/// the output, so parsing stops there.
fn parse_show(stdout: &str) -> Option<NalaPackage> {
    let mut package = NalaPackage {
        state: InstallState::Available,
        ..Default::default()
    };
    let mut in_depends = false;
    for line in stdout.lines() {
        if line.starts_with(char::is_whitespace) {
            if in_depends
                && let Some(dep) = line.split_whitespace().next()
            {
                package
                    .dependencies
                    .get_or_insert_with(Vec::new)
                    .push(dep.to_string());
            }
            continue;
        }
        in_depends = false;
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "Package" => package.name = value.to_string(),
            "Version" => package.version = Some(value.to_string()),
            "Architecture" => package.architecture = Some(value.to_string()),
            "Installed" if value == "yes" => package.state = InstallState::Installed,
            "Origin" => package.origin = Some(value.to_string()),
            "Homepage" => package.homepage = Some(value.to_string()),
            "Depends" => {
                if value.is_empty() {
                    in_depends = true;
                } else {
                    package.dependencies = Some(
                        value
                            .split(',')
                            .filter_map(|dep| dep.split_whitespace().next())
                            .map(str::to_string)
                            .collect(),
                    );
                }
            }
            "Description" => {
                if !value.is_empty() {
                    package.description = Some(value.to_string());
                }
                break;
            }
            _ => {}
        }
    }
    (!package.name.is_empty()).then_some(package)
}

/// A package as nala describes it.
#[derive(Debug, Default)]
pub struct NalaPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub architecture: Option<String>,
    pub origin: Option<String>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for NalaPackage {
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
    fn parses_installed_blocks() {
        let stdout = "\
vim 2:9.0.1378-2 [Debian/bookworm main]
├── is installed
└── Vi IMproved - enhanced vi editor

ripgrep 14.1.0-1 [Debian/bookworm main]
├── is installed
└── recursively searches directories for a regex pattern
";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "vim");
        assert_eq!(packages[0].version.as_deref(), Some("2:9.0.1378-2"));
        assert_eq!(packages[0].origin.as_deref(), Some("Debian/bookworm main"));
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(
            packages[1].description.as_deref(),
            Some("recursively searches directories for a regex pattern")
        );
    }

    #[test]
    fn parses_upgradable_block_with_ascii_glyphs() {
        // LC_ALL=C swaps the tree glyphs for ASCII.
        let stdout = "\
vim 2:8.2.3995-1+b2 [Debian/sid main]
+-- is installed and upgradable to 2:8.2.4659-1
`-- Vi IMproved - enhanced vi editor
";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].version.as_deref(), Some("2:8.2.3995-1+b2"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("2:8.2.4659-1"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn parses_upgradable_from_variant() {
        let stdout = "\
vim 2:8.2.4659-1 [Debian/sid main]
├── is upgradable from 2:8.2.3995-1+b2
└── Vi IMproved - enhanced vi editor
";
        let packages = parse_list(stdout);
        assert_eq!(packages[0].version.as_deref(), Some("2:8.2.3995-1+b2"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("2:8.2.4659-1"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn search_block_without_status_stays_available() {
        let stdout = "\
fd-find 9.0.0-1 [local]
└── simple, fast alternative to find
";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].origin.as_deref(), Some("local"));
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn parses_show_output() {
        let stdout = "\
Package: ripgrep
Version: 14.1.0-1
Architecture: amd64
Installed: yes
Priority: optional
Section: utils
Source: rust-ripgrep
Origin: Debian
Maintainer: Debian Rust Maintainers <list@debian.org>
Depends: libc6 (>= 2.34), libgcc-s1 (>= 4.2)
Homepage: https://github.com/BurntSushi/ripgrep
Download-Size: 1.4 MB
APT-Sources: http://deb.debian.org/debian bookworm/main
Description: recursively searches directories for a regex pattern
 ripgrep is a line-oriented search tool.
 Note: this continuation must not be parsed as a field.
";
        let package = parse_show(stdout).unwrap();
        assert_eq!(package.name, "ripgrep");
        assert_eq!(package.version.as_deref(), Some("14.1.0-1"));
        assert_eq!(package.origin.as_deref(), Some("Debian"));
        assert_eq!(package.state, InstallState::Installed);
        assert_eq!(
            package.dependencies,
            Some(vec!["libc6".to_string(), "libgcc-s1".to_string()])
        );
        assert_eq!(
            package.description.as_deref(),
            Some("recursively searches directories for a regex pattern")
        );
    }

    #[test]
    fn parses_show_multiline_depends() {
        // More than four dependencies wrap onto indented lines.
        let stdout = "\
Package: neovim
Version: 0.9.5-1
Installed: no
Depends: \n  libc6 (>= 2.34)
  libluajit-5.1-2 (>= 2.1)
  libmsgpackc2 (>= 4.0)
  libtermkey1 (>= 0.22)
  libuv1 (>= 1.44)
Description: heavily refactored vim fork
";
        let package = parse_show(stdout).unwrap();
        assert_eq!(
            package.dependencies,
            Some(vec![
                "libc6".to_string(),
                "libluajit-5.1-2".to_string(),
                "libmsgpackc2".to_string(),
                "libtermkey1".to_string(),
                "libuv1".to_string(),
            ])
        );
        assert_eq!(package.state, InstallState::Available);
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("ripgrep@14.1.0-1")),
            "ripgrep=14.1.0-1"
        );
        assert_eq!(spec(&PackageRequest::parse("ripgrep")), "ripgrep");
    }
}
