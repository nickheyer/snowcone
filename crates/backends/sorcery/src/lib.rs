//! Sorcery backend for snowcone.
//!
//! Source Mage spreads its interface across four commands: `cast` builds
//! and installs spells, `dispel` removes them, `gaze` answers every query,
//! and `sorcery` runs the whole-system update; `scribe update` (resolved
//! only when refresh runs, since scribe ships in the same sorcery package)
//! refreshes the installed grimoires. Every install is a source build that
//! runs as root and asks its questions on the terminal - there is no
//! yes-flag and no dry-run anywhere, so `assume_yes` has nothing to do and
//! `--dry-run` errors instead of pretending.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "sorcery";
/// Everything `create` resolves; detection probes the same set so an
/// incomplete installation is reported up front instead of failing later.
const PROGRAMS: &[&str] = &["cast", "dispel", "gaze", "sorcery"];

pub fn factory() -> Box<dyn BackendFactory> {
    Box::new(Factory)
}

struct Factory;

impl BackendFactory for Factory {
    fn id(&self) -> &'static str {
        ID
    }

    fn detect(&self, _host: &HostInfo) -> Detection {
        // All four commands are required by `create`; a partial suite is
        // unavailable, not available-and-broken.
        let mut first = None;
        for program in PROGRAMS {
            match find_program(program) {
                Some(path) => {
                    first.get_or_insert(path);
                }
                None => {
                    return Detection::Unavailable {
                        reason: format!("`{program}` not found on PATH"),
                    };
                }
            }
        }
        match first {
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
            cast: resolve("cast")?,
            dispel: resolve("dispel")?,
            gaze: resolve("gaze")?,
            sorcery: resolve("sorcery")?,
            elevator: Elevator::detect(host),
        }))
    }
}

struct Manager {
    cast: PathBuf,
    dispel: PathBuf,
    gaze: PathBuf,
    sorcery: PathBuf,
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

    /// Everything `gaze installed` reports, for listings and state probes.
    async fn installed(&self) -> Result<Vec<SorceryPackage>> {
        let output = self
            .query(&self.gaze)
            .arg("installed")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_installed(&output.stdout))
    }
}

/// The grimoire holds exactly one version per spell; there is nothing to
/// pin.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but sorcery casts the grimoire's version"
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
        "Sorcery"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "sorcery"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::SEARCH | Capabilities::REFRESH | Capabilities::UPGRADE
    }

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
        let cmd = self
            .cmd(&self.cast)
            .elevated(true)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        let cmd = self
            .cmd(&self.dispel)
            .elevated(true)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.installed().await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query(&self.gaze)
            .args(["what", name])
            .capture(&self.elevator, None)
            .await?;
        if !output.success() || output.stdout.trim().is_empty() {
            return Err(Error::NotFound(name.to_string()));
        }
        let mut package = SorceryPackage {
            name: name.to_string(),
            description: parse_what(&output.stdout, name),
            state: InstallState::Available,
            ..Default::default()
        };
        // `gaze what` only describes the grimoire entry; the installed
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
            .query(&self.gaze)
            .arg("search")
            .arg(query)
            .capture(&self.elevator, None)
            .await?;
        // gaze exits non-zero when nothing matches.
        if !output.success() && output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(boxed(parse_search(&output.stdout)))
    }

    /// `scribe update` refreshes every installed grimoire. scribe ships in
    /// the sorcery package but is not part of the four commands `create`
    /// requires, so it resolves here, with an honest error when missing;
    /// run elevated because scribe otherwise re-runs itself under `su`
    /// with its own password prompt.
    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("refresh"));
        }
        let scribe = find_program("scribe").ok_or_else(|| {
            Error::Other(format!(
                "{ID}: grimoire refresh runs `scribe update`, but `scribe` was not found on PATH"
            ))
        })?;
        let cmd = self.cmd(&scribe).arg("update").elevated(true);
        self.run(cmd, ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        if packages.is_empty() {
            let cmd = self.cmd(&self.sorcery).arg("system-update").elevated(true);
            return self.run(cmd, ctx).await;
        }
        // Casting an installed spell rebuilds it at the grimoire's version.
        let cmd = self
            .cmd(&self.cast)
            .elevated(true)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }
}

fn boxed(packages: Vec<SorceryPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `gaze installed`: one spell per line - `name version`, `name: version`,
/// or the state file's raw `name:date:status:version`, all shapes this
/// listing has appeared as across sorcery versions; prose lines are skipped.
fn parse_installed(stdout: &str) -> Vec<SorceryPackage> {
    stdout.lines().filter_map(parse_installed_line).collect()
}

/// One `gaze installed` line (see [`parse_installed`]).
fn parse_installed_line(line: &str) -> Option<SorceryPackage> {
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
    Some(SorceryPackage {
        name: name.to_string(),
        version: (!version.is_empty()).then(|| version.to_string()),
        state: InstallState::Installed,
        ..Default::default()
    })
}

/// `gaze what`: the spell's long description as free text; a leading line
/// naming the spell and dashed separator rules are skipped, and the rest is
/// folded into one line.
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

/// `gaze search`: matching spells as bare `name` lines or `name: short
/// description` lines; section headers and wrapped prose are skipped.
fn parse_search(stdout: &str) -> Vec<SorceryPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            if let Some((name, description)) = line.split_once(':') {
                let (name, description) = (name.trim(), description.trim());
                if name.is_empty() || name.contains(char::is_whitespace) {
                    return None;
                }
                return Some(SorceryPackage {
                    name: name.to_string(),
                    description: (!description.is_empty()).then(|| description.to_string()),
                    state: InstallState::Available,
                    ..Default::default()
                });
            }
            (!line.contains(char::is_whitespace)).then(|| SorceryPackage {
                name: line.to_string(),
                state: InstallState::Available,
                ..Default::default()
            })
        })
        .collect()
}

/// A package as sorcery describes it.
#[derive(Debug, Default)]
pub struct SorceryPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for SorceryPackage {
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

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_installed_word_pairs() {
        let packages = parse_installed("bash 5.2.015\nzlib 1.3\ngcc\n");
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "bash");
        assert_eq!(packages[0].version.as_deref(), Some("5.2.015"));
        assert_eq!(packages[2].version, None);
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn parses_installed_state_file_lines() {
        let stdout = "\
bash:20240101:installed:5.2.015
glibc:20231215:held:2.39
";
        let packages = parse_installed(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "glibc");
        assert_eq!(packages[1].version.as_deref(), Some("2.39"));
    }

    #[test]
    fn skips_prose_in_installed_listing() {
        let packages = parse_installed("Spells currently installed:\nno spells found\n");
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
    fn parses_search_lines() {
        let stdout = "\
bash: The GNU Bourne Again SHell
zsh
matches in long descriptions follow
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "bash");
        assert_eq!(
            packages[0].description.as_deref(),
            Some("The GNU Bourne Again SHell")
        );
        assert_eq!(packages[1].name, "zsh");
        assert_eq!(packages[1].state, InstallState::Available);
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("bash@5.2")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("bash")]).is_ok());
    }
}
