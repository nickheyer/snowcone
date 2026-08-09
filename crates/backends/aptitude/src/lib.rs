//! aptitude backend for snowcone.
//!
//! aptitude drives the dpkg database like apt does, but brings its own
//! search-pattern language (`~i`, `~U`) and display format strings, which
//! make listings parseable without guessing at column layouts. Mutations
//! run through the elevation helper; `-s` gives a faithful dry run.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "aptitude";
const PROGRAMS: &[&str] = &["aptitude"];

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
        "Aptitude"
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
            .args(["search", "--disable-columns", "-F", "%p %v %d", "~i"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_installed(&output.stdout))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query()
            .arg("show")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        // Some aptitude versions exit 0 even when the package is unknown,
        // so a missing `Package` field is the reliable "not found" signal.
        parse_show(&output.stdout)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["search", "--disable-columns", "-F", "%c %p %V %d"])
            .arg(query)
            .capture(&self.elevator, None)
            .await?;
        // No matches exits non-zero with nothing on stdout.
        if !output.success() && output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(parse_search(&output.stdout))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: refresh has no dry-run mode")));
        }
        self.run(self.cmd().arg("update").elevated(true), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = if packages.is_empty() {
            self.mutation("safe-upgrade", ctx)
        } else {
            // `install` on an installed package moves it to the candidate
            // version, which is aptitude's targeted upgrade.
            self.mutation("install", ctx)
                .args(packages.iter().map(spec))
        };
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["search", "--disable-columns", "-F", "%p %v %V %d", "~U"])
            .capture(&self.elevator, None)
            .await?;
        if !output.success() && output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(parse_outdated(&output.stdout))
    }
}

fn optional_version(version: &str) -> Option<String> {
    (version != "<none>").then(|| version.to_string())
}

/// `%p %v %d`: name, installed version, description.
fn parse_installed(stdout: &str) -> Vec<Box<dyn Package>> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, ' ');
            let name = parts.next().filter(|name| !name.is_empty())?;
            let version = parts.next()?;
            Some(Box::new(AptitudePackage {
                name: name.to_string(),
                version: optional_version(version),
                description: parts
                    .next()
                    .map(str::trim)
                    .filter(|d| !d.is_empty())
                    .map(str::to_string),
                state: InstallState::Installed,
                ..Default::default()
            }) as Box<dyn Package>)
        })
        .collect()
}

/// `%c %p %V %d`: state flag, name, candidate version, description.
fn parse_search(stdout: &str) -> Vec<Box<dyn Package>> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, ' ');
            let state = parts.next()?;
            let name = parts.next().filter(|name| !name.is_empty())?;
            let version = parts.next()?;
            Some(Box::new(AptitudePackage {
                name: name.to_string(),
                version: optional_version(version),
                description: parts
                    .next()
                    .map(str::trim)
                    .filter(|d| !d.is_empty())
                    .map(str::to_string),
                state: if state == "i" {
                    InstallState::Installed
                } else {
                    InstallState::Available
                },
                ..Default::default()
            }) as Box<dyn Package>)
        })
        .collect()
}

/// `%p %v %V %d`: name, installed version, candidate version, description.
fn parse_outdated(stdout: &str) -> Vec<Box<dyn Package>> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, ' ');
            let name = parts.next().filter(|name| !name.is_empty())?;
            let version = parts.next()?;
            let latest = parts.next()?;
            Some(Box::new(AptitudePackage {
                name: name.to_string(),
                version: optional_version(version),
                latest_version: optional_version(latest),
                description: parts
                    .next()
                    .map(str::trim)
                    .filter(|d| !d.is_empty())
                    .map(str::to_string),
                state: InstallState::Upgradable,
                ..Default::default()
            }) as Box<dyn Package>)
        })
        .collect()
}

/// `aptitude show`: `Key: Value` fields. The short description sits on the
/// `Description:` line itself; the indented long description below it is
/// deliberately not folded in.
fn parse_show(stdout: &str) -> Option<AptitudePackage> {
    let mut package = AptitudePackage::default();
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
            "Description" => package.description = Some(value.to_string()),
            "Homepage" => package.homepage = Some(value.to_string()),
            "Section" => package.origin = Some(value.to_string()),
            "Architecture" => package.architecture = Some(value.to_string()),
            "State" => {
                package.state = if value == "installed" {
                    InstallState::Installed
                } else {
                    InstallState::Available
                };
            }
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

/// A package as aptitude describes it.
#[derive(Debug, Default)]
pub struct AptitudePackage {
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

impl Package for AptitudePackage {
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
    fn parses_installed_lines() {
        let stdout =
            "bash 5.2.21-2 GNU Bourne Again SHell\nripgrep 14.1.0-1 line-oriented search tool\n";
        let packages = parse_installed(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name(), "ripgrep");
        assert_eq!(packages[1].version(), Some("14.1.0-1"));
        assert_eq!(packages[1].description(), Some("line-oriented search tool"));
        assert_eq!(packages[1].state(), InstallState::Installed);
    }

    #[test]
    fn parses_search_lines_with_state_flag() {
        let stdout = "i ripgrep 14.1.0-1 line-oriented search tool\np fd-find 9.0.0-1 simple, fast alternative to find\n";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].state(), InstallState::Installed);
        assert_eq!(packages[1].state(), InstallState::Available);
        assert_eq!(packages[1].name(), "fd-find");
    }

    #[test]
    fn parses_outdated_lines() {
        let stdout = "bash 5.2.21-2 5.2.21-3 GNU Bourne Again SHell\n";
        let packages = parse_outdated(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].version(), Some("5.2.21-2"));
        assert_eq!(packages[0].latest_version(), Some("5.2.21-3"));
        assert_eq!(packages[0].state(), InstallState::Upgradable);
    }

    #[test]
    fn parses_show_output() {
        let stdout = "\
Package: ripgrep
Version: 14.1.0-1
State: installed
Priority: optional
Section: utils
Architecture: amd64
Depends: libc6 (>= 2.34), libgcc-s1 (>= 3.0)
Description: line-oriented search tool
 ripgrep is a line-oriented search tool that recursively searches
 the current directory for a regex pattern.
Homepage: https://github.com/BurntSushi/ripgrep
";
        let package = parse_show(stdout).unwrap();
        assert_eq!(package.name, "ripgrep");
        assert_eq!(package.state, InstallState::Installed);
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
    fn missing_package_field_means_not_found() {
        assert!(parse_show("E: Unable to locate package nope\n").is_none());
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
