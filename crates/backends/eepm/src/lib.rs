//! eepm backend for snowcone.
//!
//! `epm` is ALT Linux's unified wrapper around whatever low-level package
//! manager the host actually runs, and it escalates itself: mutating
//! commands go through eepm's internal sudo detection, so snowcone never
//! prefixes an elevation helper. Output is mostly whatever the wrapped
//! manager prints (plus epm's own `$ command` echo lines), so parsing
//! sticks to the wrapper-stable parts: rpm-shaped NVR lines from `qa`,
//! `name - summary` search lines, `Key: Value` info fields, and bare names
//! from `--short list --upgradable`. Options go before the verb, matching
//! eepm's own documentation; `--auto` answers prompts non-interactively,
//! and `--dry-run` is native to remove only, with `simulate` standing in
//! as install's preview.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "eepm";
const PROGRAMS: &[&str] = &["epm"];

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

    /// Shared shape for mutating commands: epm options go before the verb.
    /// Never elevated - epm drives sudo itself.
    fn mutation(&self, verb: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd();
        if ctx.assume_yes {
            cmd = cmd.arg("--auto");
        }
        cmd.arg(verb)
    }

    fn no_dry_run(&self, operation: &str) -> Error {
        Error::Other(format!("{ID}: {operation} has no dry-run mode"))
    }
}

/// epm has no version selection of its own - installs take whatever the
/// wrapped manager resolves.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but epm only installs what the wrapped manager resolves"
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
        "eepm"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Other
    }

    fn database_id(&self) -> &'static str {
        "eepm"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::REFRESH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        let names = packages.iter().map(|package| package.name.as_str());
        if ctx.dry_run {
            // `simulate` is epm's native install preview: it resolves
            // requirements without touching the system.
            return self.run(self.cmd().arg("simulate").args(names), ctx).await;
        }
        self.run(self.mutation("install", ctx).args(names), ctx)
            .await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let mut cmd = self.cmd();
        if ctx.assume_yes {
            cmd = cmd.arg("--auto");
        }
        if ctx.dry_run {
            // Documented as remove-only among the mutating verbs.
            cmd = cmd.arg("--dry-run");
        }
        cmd = cmd
            .arg("remove")
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("qa")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_qa(&output.stdout)))
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
        let mut package =
            parse_info(&output.stdout).ok_or_else(|| Error::NotFound(name.to_string()))?;
        // `epm info` says nothing reliable about the local install; one qa
        // probe fills in state and the installed version.
        let probe = self
            .query()
            .arg("qa")
            .arg(&package.name)
            .capture(&self.elevator, None)
            .await?;
        if probe.success()
            && let Some(installed) = parse_qa(&probe.stdout)
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

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("update"));
        }
        self.run(self.mutation("update", ctx), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if packages.is_empty() {
            if ctx.dry_run {
                return Err(self.no_dry_run("upgrade"));
            }
            return self.run(self.mutation("upgrade", ctx), ctx).await;
        }
        // epm has no targeted upgrade verb; `install` follows apt semantics
        // and pulls already-installed packages up to the newest version.
        let names = packages.iter().map(|package| package.name.as_str());
        if ctx.dry_run {
            return self.run(self.cmd().arg("simulate").args(names), ctx).await;
        }
        self.run(self.mutation("install", ctx).args(names), ctx)
            .await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["--short", "list", "--upgradable"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_upgradable(&output.stdout)))
    }
}

fn boxed(packages: Vec<EepmPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// True for epm's own noise: `$`/`#` command echo lines and indented
/// continuation output from the wrapped manager.
fn is_noise(line: &str) -> bool {
    line.is_empty()
        || line.starts_with(char::is_whitespace)
        || line.starts_with('$')
        || line.starts_with('#')
}

/// `name-version-release` → name plus `version-release`, split on the last
/// two dashes (rpm forbids dashes inside version and release); tokens
/// without a digit-led version keep the whole text as the name.
fn split_nvr(token: &str) -> (&str, Option<String>) {
    let Some(release_dash) = token.rfind('-') else {
        return (token, None);
    };
    let Some(version_dash) = token[..release_dash].rfind('-') else {
        return (token, None);
    };
    let version = &token[version_dash + 1..];
    if !version.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return (token, None);
    }
    (&token[..version_dash], Some(version.to_string()))
}

/// `epm qa`: one installed package per line in rpm's NVR shape on ALT;
/// other wrapped managers may print bare names, which parse as
/// version-less.
fn parse_qa(stdout: &str) -> Vec<EepmPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            if is_noise(line) {
                return None;
            }
            let token = line.split_whitespace().next()?;
            let (name, version) = split_nvr(token);
            Some(EepmPackage {
                name: name.to_string(),
                version,
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `epm search`: `name - summary` lines (apt-cache shape on ALT); prose
/// and echo lines never match because their first field has whitespace.
fn parse_search(stdout: &str) -> Vec<EepmPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            if is_noise(line) {
                return None;
            }
            let (name, description) = line.split_once(" - ")?;
            let name = name.trim();
            if name.is_empty() || name.contains(char::is_whitespace) {
                return None;
            }
            Some(EepmPackage {
                name: name.to_string(),
                description: Some(description.trim().to_string()),
                state: InstallState::Available,
                ..Default::default()
            })
        })
        .collect()
}

/// `epm info`: `Key: Value` fields from whichever manager epm wraps -
/// rpm's separate `Version`/`Release` pair joins into one string, apt's
/// single `Version` passes through; continuation lines are ignored.
fn parse_info(stdout: &str) -> Option<EepmPackage> {
    let mut package = EepmPackage {
        state: InstallState::Available,
        ..Default::default()
    };
    let mut version = None;
    let mut release = None;
    for line in stdout.lines() {
        if is_noise(line) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if value.is_empty() {
            continue;
        }
        match key {
            "Name" | "Package" if package.name.is_empty() => package.name = value.to_string(),
            "Version" => version = Some(value.to_string()),
            "Release" => release = Some(value.to_string()),
            "Summary" | "Description" if package.description.is_none() => {
                package.description = Some(value.to_string());
            }
            "URL" | "Homepage" => package.homepage = Some(value.to_string()),
            "License" => package.license = Some(value.to_string()),
            "Architecture" => package.architecture = Some(value.to_string()),
            "Group" | "Section" => package.origin = Some(value.to_string()),
            _ => {}
        }
    }
    package.version = match (version, release) {
        (Some(version), Some(release)) => Some(format!("{version}-{release}")),
        (version, _) => version,
    };
    (!package.name.is_empty()).then_some(package)
}

/// `epm --short list --upgradable`: bare package names, one per line, with
/// epm's echo and blank-line noise in between.
fn parse_upgradable(stdout: &str) -> Vec<EepmPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            if is_noise(line) || line.trim().contains(char::is_whitespace) {
                return None;
            }
            Some(EepmPackage {
                name: line.trim().to_string(),
                state: InstallState::Upgradable,
                ..Default::default()
            })
        })
        .collect()
}

/// A package as epm (and whatever it wraps) describes it.
#[derive(Debug, Default)]
pub struct EepmPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub architecture: Option<String>,
    pub origin: Option<String>,
    pub state: InstallState,
}

impl Package for EepmPackage {
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

    fn architecture(&self) -> Option<&str> {
        self.architecture.as_deref()
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
    fn splits_nvr_tokens() {
        assert_eq!(
            split_nvr("bash-5.2.15-alt1"),
            ("bash", Some("5.2.15-alt1".to_string()))
        );
        assert_eq!(
            split_nvr("gcc-c++-13.2.1-alt2"),
            ("gcc-c++", Some("13.2.1-alt2".to_string()))
        );
        assert_eq!(split_nvr("ia32-libs"), ("ia32-libs", None));
        assert_eq!(split_nvr("bash"), ("bash", None));
    }

    #[test]
    fn parses_qa_lines_and_skips_echo_noise() {
        let stdout = "\
$ rpm -qa
bash-5.2.15-alt1
eepm-3.62.4-alt1
zsh
";
        let packages = parse_qa(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "bash");
        assert_eq!(packages[0].version.as_deref(), Some("5.2.15-alt1"));
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[2].name, "zsh");
        assert_eq!(packages[2].version, None);
    }

    #[test]
    fn parses_search_lines() {
        let stdout = "\
$ apt-cache search ripgrep
ripgrep - Recursively search directories for a regex pattern
ripgrep-all - Like ripgrep, but also search in PDFs and archives
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "ripgrep");
        assert_eq!(
            packages[0].description.as_deref(),
            Some("Recursively search directories for a regex pattern")
        );
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn parses_rpm_style_info() {
        let stdout = "\
Name        : ripgrep
Version     : 14.1.0
Release     : alt1
Architecture: x86_64
Group       : Text tools
License     : MIT
URL         : https://github.com/BurntSushi/ripgrep
Summary     : Recursively search directories for a regex pattern
Description :
ripgrep is a line-oriented search tool.
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.name, "ripgrep");
        assert_eq!(package.version.as_deref(), Some("14.1.0-alt1"));
        assert_eq!(package.origin.as_deref(), Some("Text tools"));
        assert_eq!(package.license.as_deref(), Some("MIT"));
        assert_eq!(
            package.description.as_deref(),
            Some("Recursively search directories for a regex pattern")
        );
    }

    #[test]
    fn parses_apt_style_info() {
        let stdout = "\
Package: ripgrep
Version: 14.1.0-alt1
Section: utils
Homepage: https://github.com/BurntSushi/ripgrep
Description: Recursively search directories for a regex pattern
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.name, "ripgrep");
        assert_eq!(package.version.as_deref(), Some("14.1.0-alt1"));
        assert_eq!(package.origin.as_deref(), Some("utils"));
        assert_eq!(
            package.homepage.as_deref(),
            Some("https://github.com/BurntSushi/ripgrep")
        );
    }

    #[test]
    fn parses_short_upgradable_names() {
        let stdout = "\
$ apt list --upgradable

bash
eepm
";
        let packages = parse_upgradable(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "bash");
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("ripgrep@14.1.0")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("ripgrep")]).is_ok());
    }
}
