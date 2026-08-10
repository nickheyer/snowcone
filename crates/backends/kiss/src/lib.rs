//! kiss backend for snowcone.
//!
//! kiss is KISS Linux's shell-based source manager. Installing a port
//! means building it first, so install (and targeted upgrade) run `kiss b`
//! then `kiss i`. kiss refuses to run as root and escalates itself through
//! KISS_SU where needed, so snowcone never elevates it - `needs_elevation`
//! stays true only so callers expect the credential prompt. `kiss u` pulls
//! repositories and rebuilds what changed in one verb, which is what
//! upgrade-all maps to. Prompts are silenced with kiss's own KISS_PROMPT=0
//! when `assume_yes` is set; ports carry no description metadata at all.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "kiss";
const PROGRAMS: &[&str] = &["kiss"];

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

    /// Mutating invocation - never elevated (kiss escalates itself through
    /// KISS_SU); KISS_PROMPT=0 is kiss's own way to skip prompts.
    fn mutation(&self, action: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = Cmd::new(&self.program).arg(action);
        if ctx.assume_yes {
            cmd = cmd.env("KISS_PROMPT", "0");
        }
        cmd
    }

    /// Build then install: kiss's install verb only unpacks an already
    /// built tarball, so both steps make up one snowcone install.
    async fn build_and_install(&self, names: &[&str], ctx: &OpContext) -> Result<()> {
        self.run(self.mutation("b", ctx).args(names.iter().copied()), ctx)
            .await?;
        self.run(self.mutation("i", ctx).args(names.iter().copied()), ctx)
            .await
    }

    fn no_dry_run(&self, operation: &str) -> Error {
        Error::Other(format!("{ID}: {operation} has no dry-run mode"))
    }
}

/// kiss builds whatever version the repository ports carry: nothing to pin.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but kiss builds whatever version the repository ports carry"
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
        "kiss"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "kiss"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::SEARCH | Capabilities::UPGRADE
    }

    /// kiss drives its own escalation (KISS_SU) when installing or
    /// removing - snowcone never elevates it, but a credential prompt is
    /// still coming.
    fn needs_elevation(&self, operation: Operation) -> bool {
        matches!(
            operation,
            Operation::Install | Operation::Remove | Operation::Upgrade | Operation::Refresh
        )
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        let names: Vec<&str> = packages
            .iter()
            .map(|package| package.name.as_str())
            .collect();
        self.build_and_install(&names, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        let cmd = self
            .mutation("r", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("l")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_list(&output.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query()
            .arg("s")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() || output.stdout.trim().is_empty() {
            return Err(Error::NotFound(name.to_string()));
        }
        let mut package = KissPackage {
            name: name.to_string(),
            state: InstallState::Available,
            ..Default::default()
        };
        // First repository hit: the port's origin, and its `version` file
        // for the available version (a port is a directory; the version
        // file is its only version metadata).
        let repo_hit = output
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .find_map(|line| {
                let path = Path::new(line);
                let parent = path.parent()?.file_name()?.to_str()?;
                (parent != "installed").then(|| {
                    let version = std::fs::read_to_string(path.join("version"))
                        .ok()
                        .and_then(|contents| parse_version_file(&contents));
                    (parent.to_string(), version)
                })
            });
        if let Some((origin, version)) = repo_hit {
            package.origin = Some(origin);
            package.version = version;
        }
        let list = self
            .query()
            .arg("l")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if list.success()
            && let Some(installed) = parse_list(&list.stdout)
                .into_iter()
                .find(|installed| installed.name == name)
        {
            package.state = InstallState::Installed;
            if let Some(installed_version) = installed.version {
                if package
                    .version
                    .as_ref()
                    .is_some_and(|repo| *repo != installed_version)
                {
                    package.latest_version = package.version.take();
                }
                package.version = Some(installed_version);
            }
        }
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("s")
            .arg(search_pattern(query))
            .capture(&self.elevator, None)
            .await?;
        // kiss exits non-zero when nothing matches.
        if !output.success() && output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(boxed(parse_search(&output.stdout)))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        if packages.is_empty() {
            // `kiss u` pulls every repository and rebuilds what changed -
            // refresh and upgrade are one verb in kiss.
            return self.run(self.mutation("u", ctx), ctx).await;
        }
        // No targeted upgrade verb exists; rebuilding the port is it.
        let names: Vec<&str> = packages
            .iter()
            .map(|package| package.name.as_str())
            .collect();
        self.build_and_install(&names, ctx).await
    }
}

fn boxed(packages: Vec<KissPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// Wrap a plain query in `*…*` so `kiss s` substring-matches; queries
/// already carrying glob characters pass through untouched.
fn search_pattern(query: &str) -> String {
    if query.contains(['*', '?', '[']) {
        query.to_string()
    } else {
        format!("*{query}*")
    }
}

/// `kiss l`: `name version release` per line, exactly as the port's
/// `version` file spells it (the release counter is part of the version).
fn parse_list(stdout: &str) -> Vec<KissPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut tokens = line.split_whitespace();
            let name = tokens.next()?;
            let version: Vec<&str> = tokens.collect();
            Some(KissPackage {
                name: name.to_string(),
                version: (!version.is_empty()).then(|| version.join(" ")),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `kiss s`: absolute port-directory paths, one per line; the parent
/// directory names the repository, `installed` marking the local database.
fn parse_search(stdout: &str) -> Vec<KissPackage> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let path = Path::new(line);
            let name = path.file_name()?.to_str()?.to_string();
            let parent = path
                .parent()
                .and_then(Path::file_name)
                .and_then(|parent| parent.to_str());
            let (origin, state) = match parent {
                Some("installed") => (None, InstallState::Installed),
                other => (other.map(str::to_string), InstallState::Available),
            };
            Some(KissPackage {
                name,
                origin,
                state,
                ..Default::default()
            })
        })
        .collect()
}

/// A port's `version` file: `version release` on one line, whitespace
/// normalized.
fn parse_version_file(contents: &str) -> Option<String> {
    let joined = contents.split_whitespace().collect::<Vec<_>>().join(" ");
    (!joined.is_empty()).then_some(joined)
}

/// A package as kiss describes it.
#[derive(Debug, Default)]
pub struct KissPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub origin: Option<String>,
    pub state: InstallState,
}

impl Package for KissPackage {
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
    fn parses_installed_list() {
        let packages = parse_list("zlib 1.3.1 1\nbusybox 1.36.1 1\n");
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "zlib");
        assert_eq!(packages[0].version.as_deref(), Some("1.3.1 1"));
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn parses_search_paths() {
        let stdout = "\
/var/db/kiss/repo/core/zlib
/var/db/kiss/repo/extra/zlib-ng
/var/db/kiss/installed/zlib
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "zlib");
        assert_eq!(packages[0].origin.as_deref(), Some("core"));
        assert_eq!(packages[0].state, InstallState::Available);
        assert_eq!(packages[1].origin.as_deref(), Some("extra"));
        assert_eq!(packages[2].origin, None);
        assert_eq!(packages[2].state, InstallState::Installed);
    }

    #[test]
    fn parses_version_files() {
        assert_eq!(parse_version_file("1.3.1 1\n"), Some("1.3.1 1".to_string()));
        assert_eq!(parse_version_file("  \n"), None);
    }

    #[test]
    fn wraps_plain_queries_in_globs() {
        assert_eq!(search_pattern("zlib"), "*zlib*");
        assert_eq!(search_pattern("z*"), "z*");
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("zlib@1.3.1")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("zlib")]).is_ok());
    }
}
