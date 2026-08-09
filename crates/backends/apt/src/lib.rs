//! apt backend for snowcone.
//!
//! Drives the modern `apt` CLI (not `apt-get` — the listing and show verbs
//! this backend parses are apt's own). Mutations run through the elevation
//! helper; `-s` gives a faithful dry run. The "unstable CLI" warning apt
//! prints on stderr is irrelevant here because parsing only reads stdout
//! under `LC_ALL=C`.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "apt";
const PROGRAMS: &[&str] = &["apt"];

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

    /// Shared flags for mutating commands: `-y` and the simulate switch.
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(subcommand).elevated(true);
        if ctx.assume_yes {
            cmd = cmd.arg("-y");
        }
        if ctx.dry_run {
            cmd = cmd.arg("-s");
        }
        cmd
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
        "APT"
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
        let cmd = self
            .mutation("install", ctx)
            .args(packages.iter().map(spec));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = self
            .mutation("remove", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["list", "--installed"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_list(&output.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let show = self
            .query()
            .arg("show")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !show.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        let mut package = parse_show(&show.stdout).ok_or_else(|| Error::Parse {
            what: format!("{ID} show output"),
            detail: format!("no `Package` field for `{name}`"),
        })?;
        // `apt show` never says whether the package is installed; one list
        // probe fills in state and the installed version.
        let list = self
            .query()
            .arg("list")
            .arg(&package.name)
            .capture(&self.elevator, None)
            .await?;
        if let Some(listed) = parse_list(&list.stdout)
            .into_iter()
            .find(|listed| listed.name == package.name)
        {
            match listed.state {
                InstallState::Installed => {
                    package.state = InstallState::Installed;
                    package.version = listed.version;
                }
                InstallState::Upgradable => {
                    package.state = InstallState::Upgradable;
                    package.version = listed.version;
                    package.latest_version = listed.latest_version;
                }
                _ => {}
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
            .await?
            .require_success()?;
        Ok(boxed(parse_search(&output.stdout)))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: refresh has no dry-run mode")));
        }
        self.run(self.cmd().arg("update").elevated(true), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = if packages.is_empty() {
            self.mutation("upgrade", ctx)
        } else {
            self.mutation("install", ctx)
                .arg("--only-upgrade")
                .args(packages.iter().map(spec))
        };
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["list", "--upgradable"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_list(&output.stdout)))
    }
}

fn boxed(packages: Vec<AptPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `apt list`: `name/suites version arch [markers]` lines. The bracket
/// markers carry the install state; upgradable lines name the installed
/// version as `[upgradable from: X]`.
fn parse_list(stdout: &str) -> Vec<AptPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once('/')?;
            let mut parts = rest.split_whitespace();
            let suites = parts.next()?;
            let version = parts.next()?.to_string();
            let mut package = AptPackage {
                name: name.to_string(),
                origin: Some(suites.to_string()),
                architecture: parts.next().map(str::to_string),
                state: InstallState::Available,
                ..Default::default()
            };
            if let Some(current) = line
                .split_once("[upgradable from: ")
                .map(|(_, from)| from.trim_end_matches(']').to_string())
            {
                package.version = Some(current);
                package.latest_version = Some(version);
                package.state = InstallState::Upgradable;
            } else {
                package.version = Some(version);
                if line.contains("[installed") {
                    package.state = InstallState::Installed;
                }
            }
            Some(package)
        })
        .collect()
}

/// `apt search`: `apt list`-style header lines with an indented description
/// below each.
fn parse_search(stdout: &str) -> Vec<AptPackage> {
    let mut packages: Vec<AptPackage> = Vec::new();
    for line in stdout.lines() {
        if line.starts_with(char::is_whitespace) {
            if let (Some(last), text) = (packages.last_mut(), line.trim())
                && !text.is_empty()
                && last.description.is_none()
            {
                last.description = Some(text.to_string());
            }
            continue;
        }
        packages.extend(parse_list(line));
    }
    packages
}

/// `apt show`: `Key: Value` fields; only the summary on the `Description:`
/// line itself is kept, not the indented long description.
fn parse_show(stdout: &str) -> Option<AptPackage> {
    let mut package = AptPackage {
        state: InstallState::Available,
        ..Default::default()
    };
    for line in stdout.lines() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if value.is_empty() {
            continue;
        }
        match key {
            "Package" => package.name = value.to_string(),
            "Version" => package.version = Some(value.to_string()),
            "Description" | "Description-en" => package.description = Some(value.to_string()),
            "Homepage" => package.homepage = Some(value.to_string()),
            "Section" => package.origin = Some(value.to_string()),
            "Architecture" => package.architecture = Some(value.to_string()),
            "Depends" => {
                package.dependencies = Some(
                    value
                        .split(',')
                        .filter_map(|dep| dep.split_whitespace().next())
                        .map(str::to_string)
                        .collect(),
                );
            }
            _ => {}
        }
    }
    (!package.name.is_empty()).then_some(package)
}

/// A package as apt describes it.
#[derive(Debug, Default)]
pub struct AptPackage {
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

impl Package for AptPackage {
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
    fn parses_installed_list_lines() {
        let stdout = "\
Listing...
bash/stable,now 5.2.21-2 amd64 [installed,automatic]
ripgrep/stable,now 14.1.0-1 amd64 [installed]
";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "ripgrep");
        assert_eq!(packages[1].version.as_deref(), Some("14.1.0-1"));
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn parses_upgradable_list_lines() {
        let stdout = "bash/stable 5.2.21-3 amd64 [upgradable from: 5.2.21-2]\n";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].version.as_deref(), Some("5.2.21-2"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("5.2.21-3"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn parses_search_entries() {
        let stdout = "\
Sorting...
Full Text Search...
ripgrep/stable 14.1.0-1 amd64
  line-oriented search tool

fd-find/stable 9.0.0-1 amd64 [installed]
  simple, fast alternative to find
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "ripgrep");
        assert_eq!(
            packages[0].description.as_deref(),
            Some("line-oriented search tool")
        );
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn parses_show_output() {
        let stdout = "\
Package: ripgrep
Version: 14.1.0-1
Priority: optional
Section: utils
Homepage: https://github.com/BurntSushi/ripgrep
Depends: libc6 (>= 2.34), libgcc-s1 (>= 3.0)
Description: line-oriented search tool
 ripgrep recursively searches the current directory.
";
        let package = parse_show(stdout).unwrap();
        assert_eq!(package.name, "ripgrep");
        assert_eq!(package.version.as_deref(), Some("14.1.0-1"));
        assert_eq!(
            package.description.as_deref(),
            Some("line-oriented search tool")
        );
        assert_eq!(
            package.dependencies,
            Some(vec!["libc6".to_string(), "libgcc-s1".to_string()])
        );
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
