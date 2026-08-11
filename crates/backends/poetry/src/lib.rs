//! Poetry backend for snowcone.
//!
//! Poetry is a project manager, not a system package manager: every verb
//! this backend drives (`add`, `remove`, `show`, `update`) operates on the
//! pyproject.toml project in the *current working directory*, plus
//! `search`, which queries the remote index. That cwd-scoped contract is
//! deliberate - poetry's only global surface (`poetry self`) manages
//! poetry's own plugins and is not a general install story. Reads run
//! under `--no-ansi` with LC_ALL=C so the column output parses cleanly;
//! `--dry-run` is native to add, remove, and update.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "poetry";
const PROGRAMS: &[&str] = &["poetry"];

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

    /// Read invocation with a stable locale and cleo's styling off, so the
    /// column output parses cleanly.
    fn query(&self) -> Cmd {
        Cmd::new(&self.program).env("LC_ALL", "C").arg("--no-ansi")
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

    /// Shared flags for mutating commands: `--no-interaction` and the
    /// native `--dry-run` (add, remove, and update all support both).
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(subcommand);
        if ctx.assume_yes {
            cmd = cmd.arg("--no-interaction");
        }
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        cmd
    }
}

/// `name==version` when the request pins one, bare name otherwise
/// (`poetry add` takes PEP 508-style constraints).
fn spec(request: &PackageRequest) -> String {
    match &request.version {
        Some(version) => format!("{}=={version}", request.name),
        None => request.name.clone(),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Poetry"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "python"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = self.mutation("add", ctx).args(packages.iter().map(spec));
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
            .arg("show")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_show_list(&output.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query()
            .arg("show")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        parse_show_package(&output.stdout)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("search")
            .arg(query)
            .capture(&self.elevator, None)
            .await?;
        // poetry exits non-zero on "no matches" and on sources that do not
        // support search; neither is an error worth surfacing.
        if !output.success() && output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(boxed(parse_search(&output.stdout)))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if packages.is_empty() {
            return self.run(self.mutation("update", ctx), ctx).await;
        }
        let (pinned, constrained): (Vec<&PackageRequest>, Vec<&PackageRequest>) = packages
            .iter()
            .partition(|package| package.version.is_some());
        if !constrained.is_empty() {
            let cmd = self
                .mutation("update", ctx)
                .args(constrained.iter().map(|package| package.name.as_str()));
            self.run(cmd, ctx).await?;
        }
        // `poetry update` honors the pyproject constraint; moving to a
        // pinned version means rewriting the constraint, which is `add`.
        if !pinned.is_empty() {
            let cmd = self
                .mutation("add", ctx)
                .args(pinned.iter().map(|package| spec(package)));
            self.run(cmd, ctx).await?;
        }
        Ok(())
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["show", "--outdated"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_show_outdated(&output.stdout)))
    }
}

fn boxed(packages: Vec<PoetryPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `poetry show`: `name version description…` columns; a `(!)` marker
/// after the name flags packages locked but absent from the environment.
fn parse_show_list(stdout: &str) -> Vec<PoetryPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            if line.starts_with(char::is_whitespace) {
                return None;
            }
            let mut parts = line.split_whitespace().peekable();
            let name = parts.next()?;
            let state = if parts.peek() == Some(&"(!)") {
                parts.next();
                InstallState::Available
            } else {
                InstallState::Installed
            };
            let version = parts.next()?;
            if !version.starts_with(|c: char| c.is_ascii_digit()) {
                return None;
            }
            let description = parts.collect::<Vec<_>>().join(" ");
            Some(PoetryPackage {
                name: name.to_string(),
                version: Some(version.to_string()),
                description: (!description.is_empty()).then_some(description),
                state,
                ..Default::default()
            })
        })
        .collect()
}

/// `poetry show --outdated`: `name current latest description…` columns,
/// with the same `(!)` not-installed marker as the plain listing.
fn parse_show_outdated(stdout: &str) -> Vec<PoetryPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            if line.starts_with(char::is_whitespace) {
                return None;
            }
            let mut parts = line.split_whitespace().peekable();
            let name = parts.next()?;
            if parts.peek() == Some(&"(!)") {
                parts.next();
            }
            let current = parts.next()?;
            let latest = parts.next()?;
            if !current.starts_with(|c: char| c.is_ascii_digit())
                || !latest.starts_with(|c: char| c.is_ascii_digit())
            {
                return None;
            }
            let description = parts.collect::<Vec<_>>().join(" ");
            Some(PoetryPackage {
                name: name.to_string(),
                version: Some(current.to_string()),
                latest_version: Some(latest.to_string()),
                description: (!description.is_empty()).then_some(description),
                state: InstallState::Upgradable,
                ..Default::default()
            })
        })
        .collect()
}

/// `poetry show <name>`: `key : value` lines followed by a `dependencies`
/// section of ` - name constraint` lines (and possibly `required by`).
fn parse_show_package(stdout: &str) -> Option<PoetryPackage> {
    let mut package = PoetryPackage {
        state: InstallState::Installed,
        ..Default::default()
    };
    let mut in_dependencies = false;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "dependencies" {
            in_dependencies = true;
            continue;
        }
        if trimmed == "required by" {
            in_dependencies = false;
            continue;
        }
        if in_dependencies {
            if let Some(dep) = trimmed.strip_prefix("- ")
                && let Some(name) = dep.split_whitespace().next()
            {
                package
                    .dependencies
                    .get_or_insert_with(Vec::new)
                    .push(name.to_string());
            }
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if value.is_empty() {
            continue;
        }
        match key {
            "name" => package.name = value.to_string(),
            "version" => package.version = Some(value.to_string()),
            "description" => package.description = Some(value.to_string()),
            _ => {}
        }
    }
    (!package.name.is_empty()).then_some(package)
}

/// `poetry search`: `name (version)` header lines with the description
/// indented below each.
fn parse_search(stdout: &str) -> Vec<PoetryPackage> {
    let mut packages: Vec<PoetryPackage> = Vec::new();
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
        let Some((name, rest)) = line.split_once(" (") else {
            continue;
        };
        let Some(version) = rest.strip_suffix(')') else {
            continue;
        };
        packages.push(PoetryPackage {
            name: name.trim().to_string(),
            version: Some(version.to_string()),
            state: InstallState::Available,
            ..Default::default()
        });
    }
    packages
}

/// A package as poetry describes it.
#[derive(Debug, Default)]
pub struct PoetryPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for PoetryPackage {
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
    fn parses_show_lines() {
        let stdout = "\
certifi            2024.6.2 Python package for providing Mozilla's CA Bundle.
charset-normalizer 3.3.2    The Real First Universal Charset Detector.
requests           2.32.3   Python HTTP for Humans.
";
        let packages = parse_show_list(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "certifi");
        assert_eq!(packages[0].version.as_deref(), Some("2024.6.2"));
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(
            packages[2].description.as_deref(),
            Some("Python HTTP for Humans.")
        );
    }

    #[test]
    fn show_marks_locked_but_absent_packages() {
        let packages = parse_show_list("requests (!) 2.32.3 Python HTTP for Humans.\n");
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].version.as_deref(), Some("2.32.3"));
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn parses_outdated_lines() {
        let stdout = "\
requests 2.31.0 2.32.3 Python HTTP for Humans.
urllib3  2.2.1  2.2.2  HTTP library with thread-safe connection pooling.
";
        let packages = parse_show_outdated(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].version.as_deref(), Some("2.31.0"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("2.32.3"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn parses_package_details() {
        let stdout = "\
name         : requests
version      : 2.32.3
description  : Python HTTP for Humans.

dependencies
 - certifi >=2017.4.17
 - idna >=2.5,<4
";
        let package = parse_show_package(stdout).unwrap();
        assert_eq!(package.name, "requests");
        assert_eq!(package.version.as_deref(), Some("2.32.3"));
        assert_eq!(
            package.description.as_deref(),
            Some("Python HTTP for Humans.")
        );
        assert_eq!(
            package.dependencies,
            Some(vec!["certifi".to_string(), "idna".to_string()])
        );
        assert_eq!(package.state, InstallState::Installed);
    }

    #[test]
    fn parses_search_entries() {
        let stdout = "\
requests (2.32.3)
 Python HTTP for Humans.

requests-cache (1.2.1)
 A persistent cache for python requests
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "requests");
        assert_eq!(packages[0].version.as_deref(), Some("2.32.3"));
        assert_eq!(
            packages[0].description.as_deref(),
            Some("Python HTTP for Humans.")
        );
        assert_eq!(packages[1].state, InstallState::Available);
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("requests@2.32.3")),
            "requests==2.32.3"
        );
        assert_eq!(spec(&PackageRequest::parse("requests")), "requests");
    }
}
