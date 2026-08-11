//! Cabal backend for snowcone.
//!
//! Drives cabal-install's v2 (nix-style) CLI. The store is content-addressed
//! and append-only, so cabal has no uninstall verb at all - REMOVE is not
//! advertised. Installed state comes from `cabal list --installed`, which
//! reads the GHC package databases - the only install record cabal can
//! report. cabal never prompts, but replacing an already-installed
//! executable requires `--overwrite-policy=always`, so upgrade always
//! passes it and install adds it for `assume_yes`. Version pins use the
//! documented form, a solver constraint (`--constraint="pkg==ver"`)
//! alongside the bare package target.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "cabal";
const PROGRAMS: &[&str] = &["cabal"];

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
}

/// A pinned version becomes the documented constraint flag
/// (`cabal install --constraint="bar==2.1" bar`) - cabal's target syntax
/// has no version form.
fn pin_constraint(request: &PackageRequest) -> Option<String> {
    request
        .version
        .as_ref()
        .map(|version| format!("--constraint={}=={version}", request.name))
}

/// Constraint flags for every pin, then the bare package-name targets.
fn with_targets(cmd: Cmd, packages: &[PackageRequest]) -> Cmd {
    cmd.args(packages.iter().filter_map(pin_constraint))
        .args(packages.iter().map(|request| request.name.as_str()))
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Cabal"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "haskell"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::INSTALL
            | Capabilities::LIST_INSTALLED
            | Capabilities::INFO
            | Capabilities::SEARCH
            | Capabilities::REFRESH
            | Capabilities::UPGRADE
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let mut cmd = self.cmd().arg("install");
        if ctx.assume_yes {
            cmd = cmd.arg("--overwrite-policy=always");
        }
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        self.run(with_targets(cmd, packages), ctx).await
    }

    /// cabal has no uninstall verb - the v2 store is content-addressed and
    /// append-only; removal is deleting the executable symlink from the
    /// install dir by hand.
    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        Err(self.unsupported(Operation::Remove))
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["list", "--installed", "--simple-output"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_simple_list(&output.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query()
            .arg("info")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        // `Versions installed` is part of the info block itself, so no
        // second probe is needed to fill in the install state.
        let package = parse_info(&output.stdout).ok_or_else(|| Error::Parse {
            what: format!("{ID} info output"),
            detail: format!("no `* package` header for `{name}`"),
        })?;
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("list")
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_summary(&output.stdout)))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: refresh has no dry-run mode")));
        }
        self.run(self.cmd().arg("update"), ctx).await
    }

    /// Targeted upgrade is a reinstall of the latest version over the old
    /// executable; cabal has no verb covering every installed executable,
    /// so an empty upgrade is refused rather than faked.
    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if packages.is_empty() {
            return Err(Error::Other(format!(
                "{ID}: cabal has no upgrade-all verb; name the executables to reinstall"
            )));
        }
        let mut cmd = self.cmd().args(["install", "--overwrite-policy=always"]);
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        self.run(with_targets(cmd, packages), ctx).await
    }
}

fn boxed(packages: Vec<CabalPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// The newest version in a comma-separated, ascending version list that may
/// end in an `(and N others)` elision note.
fn last_version(value: &str) -> Option<String> {
    let value = value.split('(').next().unwrap_or(value);
    value
        .split(',')
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .next_back()
        .map(str::to_string)
}

/// `cabal list --simple-output`: one `name version` pair per line.
fn parse_simple_list(stdout: &str) -> Vec<CabalPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            let version = parts
                .next()
                .filter(|version| version.starts_with(|c: char| c.is_ascii_digit()))?;
            Some(CabalPackage {
                name: name.to_string(),
                version: Some(version.to_string()),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `cabal list`: `* name` headers with `Key: Value` fields indented under
/// each; `Installed versions:` reads `[ Not installed ]` when the package
/// is absent, a version list when present.
fn parse_summary(stdout: &str) -> Vec<CabalPackage> {
    let mut packages: Vec<CabalPackage> = Vec::new();
    for line in stdout.lines() {
        if let Some(header) = line.strip_prefix("* ") {
            let Some(name) = header.split_whitespace().next() else {
                continue;
            };
            packages.push(CabalPackage {
                name: name.to_string(),
                state: InstallState::Available,
                ..Default::default()
            });
            continue;
        }
        let Some(package) = packages.last_mut() else {
            continue;
        };
        let Some((key, value)) = line.trim().split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if value.is_empty() || value.starts_with('[') {
            continue;
        }
        match key {
            "Synopsis" => package.description = Some(value.to_string()),
            "Default available version" => package.version = Some(value.to_string()),
            "Installed versions" | "Versions installed" => {
                if let Some(installed) = last_version(value) {
                    if package.version.as_deref().is_some_and(|v| v != installed) {
                        package.latest_version = package.version.take();
                        package.state = InstallState::Upgradable;
                    } else {
                        package.state = InstallState::Installed;
                    }
                    package.version = Some(installed);
                }
            }
            "Homepage" => package.homepage = Some(value.to_string()),
            "License" => package.license = Some(value.to_string()),
            _ => {}
        }
    }
    packages
}

/// `cabal info`: a `* name` header (with an optional parenthesised kind)
/// followed by `Key: Value` fields indented four spaces; values wrap onto
/// deeper-indented continuation lines, and `[ … ]` placeholders mean the
/// field has no real value.
fn parse_info(stdout: &str) -> Option<CabalPackage> {
    let mut name: Option<String> = None;
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in stdout.lines() {
        if let Some(header) = line.strip_prefix("* ") {
            if name.is_none() {
                name = header.split_whitespace().next().map(str::to_string);
            }
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 4
            && let Some((key, value)) = trimmed.split_once(':')
        {
            fields.push((key.trim().to_string(), value.trim().to_string()));
            continue;
        }
        if let Some((_, value)) = fields.last_mut() {
            value.push(' ');
            value.push_str(trimmed);
        }
    }
    let mut package = CabalPackage {
        name: name?,
        state: InstallState::Available,
        ..Default::default()
    };
    let mut available = None;
    let mut installed = None;
    for (key, value) in fields {
        if value.is_empty() || value.starts_with('[') {
            continue;
        }
        match key.as_str() {
            "Synopsis" => package.description = Some(value),
            "Description" => {
                if package.description.is_none() {
                    package.description = Some(value);
                }
            }
            "Versions available" => available = last_version(&value),
            "Versions installed" | "Installed versions" => installed = last_version(&value),
            "Homepage" => package.homepage = Some(value),
            "License" => package.license = Some(value),
            "Dependencies" => {
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
    match installed {
        Some(installed) => {
            if available.as_deref().is_some_and(|a| a != installed) {
                package.latest_version = available;
                package.state = InstallState::Upgradable;
            } else {
                package.state = InstallState::Installed;
            }
            package.version = Some(installed);
        }
        None => package.version = available,
    }
    Some(package)
}

/// A package as cabal describes it.
#[derive(Debug, Default)]
pub struct CabalPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for CabalPackage {
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

    fn license(&self) -> Option<&str> {
        self.license.as_deref()
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
    fn parses_simple_output() {
        let stdout = "\
base 4.18.2.1
optparse-applicative 0.18.1.0
Warning: this line is not a package
";
        let packages = parse_simple_list(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "optparse-applicative");
        assert_eq!(packages[1].version.as_deref(), Some("0.18.1.0"));
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn parses_list_blocks() {
        let stdout = "\
* AC-Angle
    Synopsis: Angles in degrees and radians.
    Default available version: 1.0
    Installed versions: [ Not installed ]
    License:  BSD3

* Cabal
    Synopsis: A framework for packaging Haskell software
    Default available version: 3.10.2.1
    Installed versions: 3.8.1.0
    License:  BSD3
";
        let packages = parse_summary(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "AC-Angle");
        assert_eq!(packages[0].version.as_deref(), Some("1.0"));
        assert_eq!(packages[0].state, InstallState::Available);
        assert_eq!(
            packages[0].description.as_deref(),
            Some("Angles in degrees and radians.")
        );
        assert_eq!(packages[1].version.as_deref(), Some("3.8.1.0"));
        assert_eq!(packages[1].latest_version.as_deref(), Some("3.10.2.1"));
        assert_eq!(packages[1].state, InstallState::Upgradable);
    }

    #[test]
    fn parses_info_output() {
        let stdout = "\
* zlib             (library)
    Synopsis:      Compression and decompression in the gzip and zlib
                   formats
    Versions available: 0.5.4.2, 0.6.3.0, 0.7.1.0 (and 12
                        others)
    Versions installed: 0.7.1.0
    Homepage:      https://github.com/haskell/zlib
    Bug reports:   https://github.com/haskell/zlib/issues
    License:       BSD-3-Clause
    Dependencies:  base >=4.9 && <5, bytestring >=0.10.4 && <0.13
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.name, "zlib");
        assert_eq!(package.version.as_deref(), Some("0.7.1.0"));
        assert_eq!(package.latest_version, None);
        assert_eq!(package.state, InstallState::Installed);
        assert_eq!(
            package.description.as_deref(),
            Some("Compression and decompression in the gzip and zlib formats")
        );
        assert_eq!(package.license.as_deref(), Some("BSD-3-Clause"));
        assert_eq!(
            package.dependencies,
            Some(vec!["base".to_string(), "bytestring".to_string()])
        );
    }

    #[test]
    fn info_marks_not_installed_packages_available() {
        let stdout = "\
* aeson
    Synopsis:      Fast JSON parsing and encoding
    Versions available: 0.2.0.0, 2.2.0.0, 2.2.1.0 (and 71 others)
    Versions installed: [ Not installed ]
    License:       BSD-3-Clause
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.state, InstallState::Available);
        assert_eq!(package.version.as_deref(), Some("2.2.1.0"));
        assert_eq!(package.latest_version, None);
    }

    #[test]
    fn info_marks_outdated_installs_upgradable() {
        let stdout = "\
* pandoc
    Versions available: 3.1.0, 3.2.0
    Versions installed: 3.1.0
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.state, InstallState::Upgradable);
        assert_eq!(package.version.as_deref(), Some("3.1.0"));
        assert_eq!(package.latest_version.as_deref(), Some("3.2.0"));
    }

    #[test]
    fn formats_version_pins_as_constraints() {
        assert_eq!(
            pin_constraint(&PackageRequest::parse("hlint@3.8")).as_deref(),
            Some("--constraint=hlint==3.8")
        );
        assert_eq!(pin_constraint(&PackageRequest::parse("hlint")), None);
    }
}
