//! uv backend for snowcone.
//!
//! Manages uv's tool installs (`uv tool install/uninstall/upgrade/list`,
//! the pipx equivalent): each tool gets its own venv with its entrypoints
//! linked onto PATH. That is uv's only honest system-manager surface -
//! `uv pip` and `uv add` are project territory and stay out. uv never
//! prompts, so `assume_yes` has nothing to do, and no `uv tool` verb has a
//! documented dry-run, so `dry_run` errors. `uv tool list` is the only
//! local state uv exposes and there is no registry query, so info()
//! describes installed tools only.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "uv";
const PROGRAMS: &[&str] = &["uv"];

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

    fn no_dry_run(&self, operation: &str) -> Error {
        Error::Other(format!("{ID}: {operation} has no dry-run mode"))
    }

    async fn tools(&self) -> Result<Vec<UvPackage>> {
        let output = self
            .query()
            .args(["tool", "list"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_tool_list(&output.stdout))
    }
}

/// `name==version` when the request pins one, bare name otherwise
/// (`uv tool install` takes PEP 508 requirement specifiers).
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
        "uv"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "python"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::UPGRADE | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        // `uv tool install` takes exactly one package per invocation.
        for package in packages {
            self.run(self.cmd().args(["tool", "install"]).arg(spec(package)), ctx)
                .await?;
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("uninstall"));
        }
        for package in packages {
            self.run(
                self.cmd().args(["tool", "uninstall"]).arg(&package.name),
                ctx,
            )
            .await?;
        }
        Ok(())
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .tools()
            .await?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        self.tools()
            .await?
            .into_iter()
            .find(|package| package.name == name)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        if packages.is_empty() {
            return self
                .run(self.cmd().args(["tool", "upgrade", "--all"]), ctx)
                .await;
        }
        let (pinned, latest): (Vec<&PackageRequest>, Vec<&PackageRequest>) = packages
            .iter()
            .partition(|package| package.version.is_some());
        if !latest.is_empty() {
            let cmd = self
                .cmd()
                .args(["tool", "upgrade"])
                .args(latest.iter().map(|package| package.name.as_str()));
            self.run(cmd, ctx).await?;
        }
        // `uv tool upgrade` only takes names; a pinned upgrade is a
        // reinstall with the new requirement, which re-syncs the tool venv.
        for package in pinned {
            self.run(self.cmd().args(["tool", "install"]).arg(spec(package)), ctx)
                .await?;
        }
        Ok(())
    }
}

/// `uv tool list`: `name vX.Y.Z` header lines, each followed by
/// `- entrypoint` lines; only the headers carry package data, and noise
/// like `No tools installed` fails the version check.
fn parse_tool_list(stdout: &str) -> Vec<UvPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            if line.starts_with('-') || line.starts_with(char::is_whitespace) {
                return None;
            }
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            let version = parts
                .next()?
                .strip_prefix('v')
                .filter(|version| version.starts_with(|c: char| c.is_ascii_digit()))?;
            Some(UvPackage {
                name: name.to_string(),
                version: Some(version.to_string()),
                state: InstallState::Installed,
            })
        })
        .collect()
}

/// A tool as `uv tool list` describes it.
#[derive(Debug, Default)]
pub struct UvPackage {
    pub name: String,
    pub version: Option<String>,
    pub state: InstallState,
}

impl Package for UvPackage {
    fn manager(&self) -> &str {
        ID
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tool_list() {
        let stdout = "\
black v24.4.2
- black
- blackd
ruff v0.5.0
- ruff
";
        let packages = parse_tool_list(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "black");
        assert_eq!(packages[0].version.as_deref(), Some("24.4.2"));
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[1].name, "ruff");
        assert_eq!(packages[1].version.as_deref(), Some("0.5.0"));
    }

    #[test]
    fn tool_list_noise_parses_to_nothing() {
        assert!(parse_tool_list("No tools installed\n").is_empty());
        assert!(parse_tool_list("").is_empty());
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(spec(&PackageRequest::parse("ruff@0.5.0")), "ruff==0.5.0");
        assert_eq!(spec(&PackageRequest::parse("ruff")), "ruff");
    }
}
