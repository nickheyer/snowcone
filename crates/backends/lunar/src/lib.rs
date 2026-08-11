//! Lunar backend for snowcone.
//!
//! Lunar Linux (a Sorcery descendant) splits its interface the same way its
//! ancestor does: `lin` compiles and installs modules, `lrm` removes them,
//! `lvu` answers queries against moonbase, and `lunar` renovates the whole
//! system. Every install is a source build that runs as root and prompts on
//! the terminal - no yes-flag, no dry-run - so `assume_yes` has nothing to
//! do and `--dry-run` errors instead of pretending. Version pins go through
//! `lin -w <version>` ("Try to install a different version that is not in
//! moonbase"), which exports WANT_VERSION for the build.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "lunar";
const PROGRAMS: &[&str] = &["lunar"];

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
        let resolve =
            |name: &str| find_program(name).ok_or_else(|| Error::Unavailable(ID.to_string()));
        Ok(Box::new(Manager {
            lunar: resolve("lunar")?,
            lin: resolve("lin")?,
            lrm: resolve("lrm")?,
            lvu: resolve("lvu")?,
            elevator: Elevator::detect(host),
        }))
    }
}

struct Manager {
    lunar: PathBuf,
    lin: PathBuf,
    lrm: PathBuf,
    lvu: PathBuf,
    elevator: Elevator,
}

impl Manager {
    /// Mutating invocation, in the user's locale (output is passed through).
    fn cmd(&self, program: &Path) -> Cmd {
        Cmd::new(program)
    }

    /// Read invocation with a stable locale, so parsing survives i18n.
    fn query(&self, program: &Path) -> Cmd {
        Cmd::new(program).env("LC_ALL", "C")
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

    /// Build modules with `lin`. `-w` exports WANT_VERSION for the whole
    /// invocation (prog/lin: `export WANT_VERSION=$2`), so each pinned
    /// request builds in its own invocation; unpinned requests share one.
    async fn lin_build(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let unpinned: Vec<&str> = packages
            .iter()
            .filter(|package| package.version.is_none())
            .map(|package| package.name.as_str())
            .collect();
        if !unpinned.is_empty() {
            let cmd = self.cmd(&self.lin).elevated(true).args(unpinned);
            self.run(cmd, ctx).await?;
        }
        for package in packages {
            let Some(version) = &package.version else {
                continue;
            };
            let cmd = self
                .cmd(&self.lin)
                .elevated(true)
                .args(["-w", version])
                .arg(&package.name);
            self.run(cmd, ctx).await?;
        }
        Ok(())
    }

    /// Everything `lvu installed` reports, for listings and state probes.
    async fn installed(&self) -> Result<Vec<LunarPackage>> {
        let output = self
            .query(&self.lvu)
            .arg("installed")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_installed(&output.stdout))
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Lunar"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "lunar"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::REFRESH
            | Capabilities::UPGRADE
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
        self.lin_build(packages, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        let cmd = self
            .cmd(&self.lrm)
            .elevated(true)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.installed().await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query(&self.lvu)
            .args(["what", name])
            .capture(&self.elevator, None)
            .await?;
        if !output.success() || output.stdout.trim().is_empty() {
            return Err(Error::NotFound(name.to_string()));
        }
        let mut package = LunarPackage {
            name: name.to_string(),
            description: parse_what(&output.stdout, name),
            state: InstallState::Available,
            ..Default::default()
        };
        // `lvu what` only describes the moonbase entry; the installed
        // listing fills in state and version.
        if let Some(installed) = self
            .installed()
            .await?
            .into_iter()
            .find(|installed| installed.name == package.name)
        {
            package.state = InstallState::Installed;
            package.version = installed.version;
        }
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query(&self.lvu)
            .arg("search")
            .arg(query)
            .capture(&self.elevator, None)
            .await?;
        // lvu exits non-zero when nothing matches.
        if !output.success() && output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(boxed(parse_search(&output.stdout)))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("refresh"));
        }
        // `lin moonbase` fetches the fresh module database.
        self.run(self.cmd(&self.lin).arg("moonbase").elevated(true), ctx)
            .await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        if packages.is_empty() {
            let cmd = self.cmd(&self.lunar).arg("update").elevated(true);
            return self.run(cmd, ctx).await;
        }
        // `lin` on an installed module rebuilds it at moonbase's version,
        // or at the pinned version via `-w`.
        self.lin_build(packages, ctx).await
    }
}

fn boxed(packages: Vec<LunarPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `lvu installed`: one module per line - `name version`, `name: version`,
/// or the state file's raw `name:date:status:version`, all shapes this
/// listing has appeared as across lunar versions; prose lines are skipped.
fn parse_installed(stdout: &str) -> Vec<LunarPackage> {
    stdout.lines().filter_map(parse_installed_line).collect()
}

/// One `lvu installed` line (see [`parse_installed`]).
fn parse_installed_line(line: &str) -> Option<LunarPackage> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let looks_versioned = |version: &str| version.starts_with(|c: char| c.is_ascii_digit());
    let (name, version) = if line.contains(':') {
        let fields: Vec<&str> = line.split(':').collect();
        match fields.as_slice() {
            [name, _, _, version] => (name.trim(), version.trim()),
            [name, version] if looks_versioned(version.trim()) => (name.trim(), version.trim()),
            _ => return None,
        }
    } else {
        let mut parts = line.split_whitespace();
        let name = parts.next()?;
        match parts.next() {
            Some(version) if looks_versioned(version) => (name, version),
            Some(_) => return None,
            None => (name, ""),
        }
    };
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    Some(LunarPackage {
        name: name.to_string(),
        version: (!version.is_empty()).then(|| version.to_string()),
        state: InstallState::Installed,
        ..Default::default()
    })
}

/// `lvu what`: the module's long description as free text; a leading line
/// naming the module and dashed separator rules are skipped, and the rest
/// is folded into one line.
fn parse_what(stdout: &str, name: &str) -> Option<String> {
    let mut kept: Vec<&str> = Vec::new();
    for line in stdout.lines() {
        let text = line.trim();
        if text.is_empty() || text.chars().all(|c| matches!(c, '-' | '=')) {
            continue;
        }
        if kept.is_empty() && text.trim_end_matches(':') == name {
            continue;
        }
        kept.push(text);
    }
    (!kept.is_empty()).then(|| kept.join(" "))
}

/// `lvu search`: matching module names, one per line, sometimes prefixed
/// with their moonbase section (`section/name`); prose lines are skipped.
fn parse_search(stdout: &str) -> Vec<LunarPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.contains(char::is_whitespace) {
                return None;
            }
            let (origin, name) = match line.rsplit_once('/') {
                Some((section, name)) => (Some(section.to_string()), name),
                None => (None, line),
            };
            if name.is_empty() {
                return None;
            }
            Some(LunarPackage {
                name: name.to_string(),
                origin,
                state: InstallState::Available,
                ..Default::default()
            })
        })
        .collect()
}

/// A package as lunar describes it.
#[derive(Debug, Default)]
pub struct LunarPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub origin: Option<String>,
    pub state: InstallState,
}

impl Package for LunarPackage {
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
    fn parses_installed_word_pairs() {
        let packages = parse_installed("bash 5.2.21\nzlib 1.3.1\ngcc\n");
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "bash");
        assert_eq!(packages[0].version.as_deref(), Some("5.2.21"));
        assert_eq!(packages[2].version, None);
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn parses_installed_state_file_lines() {
        let stdout = "\
bash:20240101:installed:5.2.21
glibc:20231215:held:2.39
";
        let packages = parse_installed(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "glibc");
        assert_eq!(packages[1].version.as_deref(), Some("2.39"));
    }

    #[test]
    fn skips_prose_in_installed_listing() {
        let packages = parse_installed("Modules currently installed:\nno modules found\n");
        assert!(packages.is_empty());
    }

    #[test]
    fn parses_what_description() {
        let stdout = "bash\n----\nThe GNU Bourne Again SHell,\nan sh-compatible shell.\n";
        assert_eq!(
            parse_what(stdout, "bash").as_deref(),
            Some("The GNU Bourne Again SHell, an sh-compatible shell.")
        );
    }

    #[test]
    fn parses_search_names_and_sections() {
        let stdout = "\
utils/bash
zsh
nothing matched your query
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "bash");
        assert_eq!(packages[0].origin.as_deref(), Some("utils"));
        assert_eq!(packages[1].name, "zsh");
        assert_eq!(packages[1].origin, None);
        assert_eq!(packages[1].state, InstallState::Available);
    }
}
