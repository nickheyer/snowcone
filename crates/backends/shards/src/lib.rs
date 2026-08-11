//! Shards backend for snowcone.
//!
//! Crystal's `shards` is strictly project-scoped: every verb works on the
//! `shard.yml` in the current directory and installs into its `lib/`.
//! Dependencies are declared by editing `shard.yml` - there is no
//! install/remove-by-name - so `install` with no package arguments installs
//! the declared dependencies, and named installs or removes fail with an
//! explanatory error. `shards update` upgrades declared dependencies (all
//! of them, or just the named ones) within shard.yml's requirements, and
//! `shards outdated` reports newer versions. Reads parse `shards list`,
//! which prints every installed shard (transitive dependencies included)
//! and fails when the project's dependencies are not yet installed.
//! `shards` never prompts, so `assume_yes` has nothing to do, and nothing
//! needs elevation.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "shards";
const PROGRAMS: &[&str] = &["shards"];

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

    /// The current project's installed shards, from `shards list`.
    async fn installed(&self) -> Result<Vec<ShardsPackage>> {
        let output = self
            .query()
            .arg("list")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_list(&output.stdout))
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Shards"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "shards"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::UPGRADE | Capabilities::LIST_OUTDATED
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if let Some(package) = packages.first() {
            return Err(Error::Other(format!(
                "{ID}: cannot add `{}` by name - shards installs the dependencies declared \
                 in shard.yml; edit shard.yml, then run install with no package arguments",
                package.name
            )));
        }
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: install has no dry-run mode")));
        }
        self.run(self.cmd().arg("install"), ctx).await
    }

    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        Err(Error::Other(format!(
            "{ID}: shards has no remove verb - delete the dependency from shard.yml, \
             then run `shards prune` to drop unused installs"
        )))
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .installed()
            .await?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        self.installed()
            .await?
            .into_iter()
            .find(|package| package.name == name)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if let Some(package) = packages.iter().find(|package| package.version.is_some()) {
            return Err(Error::Other(format!(
                "{ID}: `{package}` cannot be pinned on the command line - shard.yml \
                 owns the version requirement"
            )));
        }
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: upgrade has no dry-run mode")));
        }
        // `shards update` with names updates only those dependencies,
        // staying conservative with the rest of the lock file.
        self.run(
            self.cmd()
                .arg("update")
                .args(packages.iter().map(|package| package.name.as_str())),
            ctx,
        )
        .await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("outdated")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_outdated(&output.stdout)
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }
}

/// `shards list`: a `Shards installed:` header, then one `* name (version)`
/// line per installed shard; everything inside the parentheses is the
/// version.
fn parse_list(stdout: &str) -> Vec<ShardsPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("* ")?;
            let (name, rest) = rest.split_once(" (")?;
            let version = rest.strip_suffix(')')?;
            Some(ShardsPackage {
                name: name.to_string(),
                version: Some(version.to_string()),
                latest_version: None,
                state: InstallState::Installed,
            })
        })
        .collect()
}

/// `shards outdated`: status lines go to stderr; each outdated shard gets a
/// stdout line of `  * name (installed: X[, available: Y][, latest: Z])`,
/// where `available` appears when the resolvable version differs from the
/// installed one and `latest` when a newer release is blocked by the
/// shard.yml requirement.
fn parse_outdated(stdout: &str) -> Vec<ShardsPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("* ")?;
            let (name, rest) = rest.split_once(" (")?;
            let fields = rest.strip_suffix(')')?;
            let mut installed = None;
            let mut available = None;
            let mut latest = None;
            for field in fields.split(", ") {
                match field.split_once(": ") {
                    Some(("installed", value)) => installed = Some(value.to_string()),
                    Some(("available", value)) => available = Some(value.to_string()),
                    Some(("latest", value)) => latest = Some(value.to_string()),
                    _ => {}
                }
            }
            Some(ShardsPackage {
                name: name.to_string(),
                version: installed,
                latest_version: available.or(latest),
                state: InstallState::Upgradable,
            })
        })
        .collect()
}

/// A shard as `shards list` describes it.
#[derive(Debug, Default)]
pub struct ShardsPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub state: InstallState,
}

impl Package for ShardsPackage {
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

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_list_output() {
        let stdout = "\
Shards installed:
  * ameba (1.6.1)
  * kemal (1.5.0)
  * radix (0.4.1)
";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "ameba");
        assert_eq!(packages[0].version.as_deref(), Some("1.6.1"));
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[2].name, "radix");
    }

    #[test]
    fn header_only_output_parses_to_nothing() {
        assert!(parse_list("Shards installed:\n").is_empty());
        assert!(parse_list("").is_empty());
    }

    #[test]
    fn keeps_the_whole_parenthesized_version() {
        let packages = parse_list("  * mydep (0.1.0 at 4f2c9a1)\n");
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].version.as_deref(), Some("0.1.0 at 4f2c9a1"));
    }

    #[test]
    fn parses_outdated_entries() {
        let stdout = "\
  * ameba (installed: 1.6.1, available: 1.6.4)
  * kemal (installed: 1.4.0, available: 1.5.0, latest: 1.6.0)
";
        let packages = parse_outdated(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "ameba");
        assert_eq!(packages[0].version.as_deref(), Some("1.6.1"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("1.6.4"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
        assert_eq!(packages[1].latest_version.as_deref(), Some("1.5.0"));
    }

    #[test]
    fn outdated_without_entries_parses_to_nothing() {
        assert!(parse_outdated("").is_empty());
    }
}
