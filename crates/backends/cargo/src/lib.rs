//! Cargo backend for snowcone.
//!
//! Manages `cargo install`-ed binaries: crates built from the registry into
//! $CARGO_HOME/bin. cargo never prompts, so `assume_yes` has nothing to do,
//! and stable cargo has no simulate flag - every mutation errors under
//! `--dry-run`. Upgrading is reinstalling: `cargo install` rebuilds an
//! installed crate only when it is out of date, and the whole-set upgrade
//! skips git/path installs so their source is never silently switched to
//! crates.io. Info comes from `cargo search` (exact match) plus an
//! installed-list probe, because `cargo info` only exists on cargo 1.82+.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "cargo";
const PROGRAMS: &[&str] = &["cargo"];

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

    async fn installed(&self) -> Result<Vec<CargoPackage>> {
        let output = self
            .query()
            .args(["install", "--list"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_install_list(&output.stdout))
    }
}

/// `name@version` when the request pins one, bare name otherwise.
fn spec(request: &PackageRequest) -> String {
    match &request.version {
        Some(version) => format!("{}@{version}", request.name),
        None => request.name.clone(),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Cargo"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "cargo"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::UPGRADE
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: install has no dry-run mode")));
        }
        let cmd = self.cmd().arg("install").args(packages.iter().map(spec));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: remove has no dry-run mode")));
        }
        let cmd = self
            .cmd()
            .arg("uninstall")
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.installed().await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query()
            .arg("search")
            .arg(name)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        let mut package = parse_search(&output.stdout)
            .into_iter()
            .find(|package| package.name == name)
            .ok_or_else(|| Error::NotFound(name.to_string()))?;
        // The registry view says nothing about the local install.
        if let Some(installed) = self
            .installed()
            .await?
            .into_iter()
            .find(|installed| installed.name == package.name)
        {
            package.state = InstallState::Installed;
            if installed.version.is_some() && installed.version != package.version {
                package.latest_version = package.version.take();
                package.version = installed.version;
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

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: upgrade has no dry-run mode")));
        }
        let targets: Vec<String> = if packages.is_empty() {
            // Reinstalling is cargo's upgrade; git/path installs are skipped
            // so their source is not silently switched to crates.io.
            self.installed()
                .await?
                .into_iter()
                .filter(|package| package.origin.is_none())
                .map(|package| package.name)
                .collect()
        } else {
            packages.iter().map(spec).collect()
        };
        if targets.is_empty() {
            return Ok(());
        }
        self.run(self.cmd().arg("install").args(targets), ctx).await
    }
}

fn boxed(packages: Vec<CargoPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `cargo install --list`: unindented `name vX.Y.Z[ (source)]:` headers with
/// the crate's binaries indented below; registry installs carry no source.
fn parse_install_list(stdout: &str) -> Vec<CargoPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            if line.starts_with(char::is_whitespace) {
                return None;
            }
            let header = line.strip_suffix(':')?;
            let mut parts = header.split_whitespace();
            let name = parts.next()?;
            let version = parts.next()?.strip_prefix('v')?;
            let origin = header
                .split_once(" (")
                .and_then(|(_, rest)| rest.strip_suffix(')'))
                .map(str::to_string);
            Some(CargoPackage {
                name: name.to_string(),
                version: Some(version.to_string()),
                origin,
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `cargo search`: `name = "1.2.3"    # description` lines, with a trailing
/// `... and N crates more` note to skip.
fn parse_search(stdout: &str) -> Vec<CargoPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let (spec, description) = match line.split_once('#') {
                Some((spec, description)) => (spec, Some(description.trim())),
                None => (line, None),
            };
            let (name, version) = spec.split_once('=')?;
            let name = name.trim();
            if name.is_empty() || name.contains(char::is_whitespace) {
                return None;
            }
            let version = version.trim().strip_prefix('"')?.strip_suffix('"')?;
            Some(CargoPackage {
                name: name.to_string(),
                version: Some(version.to_string()),
                description: description
                    .filter(|text| !text.is_empty())
                    .map(str::to_string),
                state: InstallState::Available,
                ..Default::default()
            })
        })
        .collect()
}

/// A package as cargo describes it.
#[derive(Debug, Default)]
pub struct CargoPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    /// Git URL or local path for non-registry installs; `None` means
    /// crates.io.
    pub origin: Option<String>,
    pub state: InstallState,
}

impl Package for CargoPackage {
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
    fn parses_install_list() {
        let stdout = "\
cargo-edit v0.12.2:
    cargo-add
    cargo-rm
    cargo-upgrade
ripgrep v14.1.1:
    rg
sccache v0.8.1 (/home/nick/src/sccache):
    sccache
tealdeer v1.6.1 (https://github.com/dbrgn/tealdeer#4b2f6ba1):
    tldr
";
        let packages = parse_install_list(stdout);
        assert_eq!(packages.len(), 4);
        assert_eq!(packages[0].name, "cargo-edit");
        assert_eq!(packages[0].version.as_deref(), Some("0.12.2"));
        assert_eq!(packages[0].origin, None);
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(
            packages[2].origin.as_deref(),
            Some("/home/nick/src/sccache")
        );
        assert_eq!(
            packages[3].origin.as_deref(),
            Some("https://github.com/dbrgn/tealdeer#4b2f6ba1")
        );
    }

    #[test]
    fn parses_search_lines() {
        let stdout = "\
ripgrep = \"14.1.1\"        # ripgrep is a line-oriented search tool that recursively searches the current directory for a regex pattern.
grep-searcher = \"0.1.13\"    # Fast line oriented regex searching as a library.
quiet-crate = \"0.1.0\"
... and 1795 crates more (use --limit N to see more)
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "ripgrep");
        assert_eq!(packages[0].version.as_deref(), Some("14.1.1"));
        assert!(
            packages[0]
                .description
                .as_deref()
                .is_some_and(|text| text.starts_with("ripgrep is a line-oriented"))
        );
        assert_eq!(packages[2].name, "quiet-crate");
        assert_eq!(packages[2].description, None);
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn search_trailer_with_url_is_skipped() {
        // Old cargos put a search URL (containing `=`) in the trailer.
        let stdout =
            "... and 42 crates more (go to https://crates.io/search?q=serde to see more)\n";
        assert!(parse_search(stdout).is_empty());
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("ripgrep@14.1.1")),
            "ripgrep@14.1.1"
        );
        assert_eq!(spec(&PackageRequest::parse("ripgrep")), "ripgrep");
    }
}
