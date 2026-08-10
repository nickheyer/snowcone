//! Dart pub backend for snowcone.
//!
//! Manages `pub global` activations - per-project pubspec dependencies
//! belong to project tooling, not a system package CLI. Drives whichever
//! of `dart`/`flutter` is on PATH (dart preferred); both expose the same
//! `pub global` verbs. pub.dev has no CLI search or info verb, so info
//! answers from the activated list alone, and there is no global outdated
//! listing at all - that stub capability is dropped. Upgrading is pub's
//! documented flow of activating again. pub never prompts and has no
//! dry-run mode, and activate/deactivate take one package per invocation,
//! so batch operations loop.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "pub";
const PROGRAMS: &[&str] = &["dart", "flutter"];

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
                reason: format!("none of {PROGRAMS:?} found on PATH"),
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
        Cmd::new(&self.program).args(["pub", "global"])
    }

    /// Read invocation with a stable locale, so parsing survives i18n.
    fn query(&self) -> Cmd {
        Cmd::new(&self.program)
            .args(["pub", "global"])
            .env("LC_ALL", "C")
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

    /// Globally activated packages, from `pub global list`.
    async fn global_list(&self) -> Result<Vec<PubPackage>> {
        let output = self
            .query()
            .arg("list")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_list(&output.stdout))
    }
}

/// `activate` takes a pinned version as a separate positional constraint
/// after the package name.
fn spec(request: &PackageRequest) -> Vec<String> {
    let mut args = vec![request.name.clone()];
    if let Some(version) = &request.version {
        args.push(version.clone());
    }
    args
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Dart pub"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "pub"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::UPGRADE | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        for package in packages {
            self.run(self.cmd().arg("activate").args(spec(package)), ctx)
                .await?;
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        for package in packages {
            self.run(self.cmd().arg("deactivate").arg(&package.name), ctx)
                .await?;
        }
        Ok(())
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .global_list()
            .await?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        self.global_list()
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
        // No upgrade verb: activating again fetches the newest version.
        if packages.is_empty() {
            for package in self.global_list().await? {
                self.run(self.cmd().arg("activate").arg(&package.name), ctx)
                    .await?;
            }
            return Ok(());
        }
        for package in packages {
            self.run(self.cmd().arg("activate").args(spec(package)), ctx)
                .await?;
        }
        Ok(())
    }
}

/// `pub global list`: `name version` lines; path- and git-activated
/// packages append `at path "…"` / a repository note, which is ignored.
fn parse_list(stdout: &str) -> Vec<PubPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            Some(PubPackage {
                name: name.to_string(),
                version: parts.next().map(str::to_string),
                state: InstallState::Installed,
            })
        })
        .collect()
}

/// A package as pub describes it - `pub global list` yields only a name
/// and version.
#[derive(Debug, Default)]
pub struct PubPackage {
    pub name: String,
    pub version: Option<String>,
    pub state: InstallState,
}

impl Package for PubPackage {
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
    fn parses_global_list() {
        let stdout = "\
devtools 2.30.0
stagehand 3.3.11
";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "devtools");
        assert_eq!(packages[0].version.as_deref(), Some("2.30.0"));
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn list_ignores_activation_source_notes() {
        let stdout = "my_tool 1.0.0 at path \"/home/nick/code/my_tool\"\n";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "my_tool");
        assert_eq!(packages[0].version.as_deref(), Some("1.0.0"));
    }

    #[test]
    fn formats_version_pins_as_positional_args() {
        assert_eq!(
            spec(&PackageRequest::parse("devtools@2.30.0")),
            vec!["devtools".to_string(), "2.30.0".to_string()]
        );
        assert_eq!(
            spec(&PackageRequest::parse("devtools")),
            vec!["devtools".to_string()]
        );
    }
}
