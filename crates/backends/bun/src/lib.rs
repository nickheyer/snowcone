//! Bun backend for snowcone.
//!
//! Manages bun's global installs (`bun add -g`), the same user-level stance
//! as the npm backend. The global inventory comes from `bun pm ls -g`, a
//! tree print of the global install dir parsed line by line - bun has no
//! machine-readable listing. Bun's installer never prompts, so `assume_yes`
//! has nothing to do. `--dry-run` is documented for `bun add`; remove and
//! whole-tree update carry no such certainty, so they error under dry-run
//! instead of guessing. Registry metadata is left alone (`bun info` is too
//! new to rely on), so `info` only describes installed globals.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "bun";
const PROGRAMS: &[&str] = &["bun"];

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

    /// Globally installed packages, from `bun pm ls -g`.
    async fn global_list(&self) -> Result<Vec<BunPackage>> {
        let output = self
            .query()
            .args(["pm", "ls", "-g"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_tree(&output.stdout))
    }
}

/// `name@version` when the request pins one, bare name otherwise.
fn spec(request: &PackageRequest) -> String {
    match &request.version {
        Some(version) => format!("{}@{version}", request.name),
        None => request.name.clone(),
    }
}

/// Split `name@version`; a leading `@` (npm scopes) belongs to the name.
fn split_spec(spec: &str) -> (String, Option<String>) {
    match spec.char_indices().skip(1).find(|&(_, c)| c == '@') {
        Some((at, _)) => (
            spec[..at].to_string(),
            Some(spec[at + 1..].to_string()).filter(|version| !version.is_empty()),
        ),
        None => (spec.to_string(), None),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Bun"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "node"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::UPGRADE | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let mut cmd = self.cmd().args(["add", "-g"]);
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        self.run(cmd.args(packages.iter().map(spec)), ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        let cmd = self
            .cmd()
            .args(["remove", "-g"])
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
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
        if packages.is_empty() {
            if ctx.dry_run {
                return Err(self.no_dry_run("upgrade"));
            }
            return self.run(self.cmd().args(["update", "-g"]), ctx).await;
        }
        // `add name@latest` upgrades past whatever semver range the
        // original install recorded; `update` would respect it.
        let mut cmd = self.cmd().args(["add", "-g"]);
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        cmd = cmd.args(packages.iter().map(|package| match &package.version {
            Some(version) => format!("{}@{version}", package.name),
            None => format!("{}@latest", package.name),
        }));
        self.run(cmd, ctx).await
    }
}

/// `bun pm ls`: a root line naming the install dir, then one
/// `├── name@version` / `└── name@version` branch per package.
fn parse_tree(stdout: &str) -> Vec<BunPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let (_, entry) = line.split_once("── ")?;
            let (name, version) = split_spec(entry.trim());
            Some(BunPackage {
                name,
                version,
                state: InstallState::Installed,
            })
        })
        .collect()
}

/// A package as bun describes it.
#[derive(Debug)]
pub struct BunPackage {
    pub name: String,
    pub version: Option<String>,
    pub state: InstallState,
}

impl Package for BunPackage {
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
    fn parses_global_tree() {
        let stdout = "\
/home/nick/.bun/install/global node_modules (3)
├── @angular/cli@17.3.8
├── prettier@3.3.2
└── typescript@5.5.3
";
        let packages = parse_tree(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "@angular/cli");
        assert_eq!(packages[0].version.as_deref(), Some("17.3.8"));
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[2].name, "typescript");
        assert_eq!(packages[2].version.as_deref(), Some("5.5.3"));
    }

    #[test]
    fn empty_tree_parses_to_nothing() {
        assert!(parse_tree("/home/nick/.bun/install/global node_modules\n").is_empty());
    }

    #[test]
    fn splits_specs_keeping_scopes() {
        assert_eq!(
            split_spec("@scope/pkg@1.0.0"),
            ("@scope/pkg".to_string(), Some("1.0.0".to_string()))
        );
        assert_eq!(split_spec("prettier"), ("prettier".to_string(), None));
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("typescript@5.5.3")),
            "typescript@5.5.3"
        );
        assert_eq!(spec(&PackageRequest::parse("prettier")), "prettier");
    }
}
