//! aura backend for snowcone.
//!
//! aura shares pacman's alpm database but, unlike the other AUR helpers, it
//! separates repo operations (`-S`, passed through to pacman) from AUR
//! operations (`-A`). Unified operations therefore classify each package
//! first: a `-Si` hit means repo, anything else is treated as AUR. aura
//! escalates through sudo on its own — snowcone never elevates it, because
//! makepkg refuses to run as root.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "aura";
const PROGRAMS: &[&str] = &["aura"];

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

    fn no_dry_run(&self, operation: &str) -> Error {
        Error::Other(format!("{ID}: {operation} has no reliable dry-run mode"))
    }

    /// Split names into (repo, aur): a `-Si` hit means the sync repos know
    /// the package, anything else is assumed to live in the AUR.
    async fn classify<'a>(&self, names: &[&'a str]) -> Result<(Vec<&'a str>, Vec<&'a str>)> {
        let mut repo = Vec::new();
        let mut aur = Vec::new();
        for &name in names {
            let probe = self
                .query()
                .arg("-Si")
                .arg(name)
                .capture(&self.elevator, None)
                .await?;
            if probe.success() {
                repo.push(name);
            } else {
                aur.push(name);
            }
        }
        Ok((repo, aur))
    }
}

/// alpm has no version selection: installs always take the repo/AUR head.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but alpm only installs the latest"
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
        "Aura"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "alpm"
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
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        let names: Vec<&str> = packages
            .iter()
            .map(|package| package.name.as_str())
            .collect();
        let (repo, aur) = self.classify(&names).await?;
        if !repo.is_empty() {
            let mut cmd = self.cmd().arg("-S").arg("--needed");
            if ctx.assume_yes {
                cmd = cmd.arg("--noconfirm");
            }
            self.run(cmd.args(repo), ctx).await?;
        }
        if !aur.is_empty() {
            let mut cmd = self.cmd().arg("-A");
            if ctx.assume_yes {
                cmd = cmd.arg("--noconfirm");
            }
            self.run(cmd.args(aur), ctx).await?;
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        let mut cmd = self.cmd().arg("-R");
        if ctx.assume_yes {
            cmd = cmd.arg("--noconfirm");
        }
        cmd = cmd.args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("-Q")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_installed(&output.stdout))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // Installed → sync repos → AUR, first hit wins.
        let attempts: [(&str, InstallState); 3] = [
            ("-Qi", InstallState::Installed),
            ("-Si", InstallState::Available),
            ("-Ai", InstallState::Available),
        ];
        for (flag, state) in attempts {
            let output = self
                .query()
                .arg(flag)
                .arg(name)
                .capture(&self.elevator, None)
                .await?;
            if !output.success() {
                continue;
            }
            if let Some(package) = parse_info(&output.stdout, state) {
                return Ok(Box::new(package));
            }
        }
        Err(Error::NotFound(name.to_string()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        // Repo results and AUR results come from separate flags.
        let mut packages = Vec::new();
        for flag in ["-Ss", "-As"] {
            let output = self
                .query()
                .arg(flag)
                .arg(query)
                .capture(&self.elevator, None)
                .await?;
            // pacman-style tools exit non-zero on "no matches".
            if output.success() || !output.stdout.trim().is_empty() {
                packages.extend(parse_search(&output.stdout));
            }
        }
        Ok(packages)
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("refresh"));
        }
        self.run(self.cmd().arg("-Sy"), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        if packages.is_empty() {
            // Repo upgrade first, then the AUR pass — aura has no single
            // "upgrade everything" flag.
            let mut sync = self.cmd().arg("-Syu");
            let mut aur = self.cmd().arg("-Au");
            if ctx.assume_yes {
                sync = sync.arg("--noconfirm");
                aur = aur.arg("--noconfirm");
            }
            self.run(sync, ctx).await?;
            return self.run(aur, ctx).await;
        }
        let names: Vec<&str> = packages
            .iter()
            .map(|package| package.name.as_str())
            .collect();
        let (repo, aur) = self.classify(&names).await?;
        if !repo.is_empty() {
            let mut cmd = self.cmd().arg("-S");
            if ctx.assume_yes {
                cmd = cmd.arg("--noconfirm");
            }
            self.run(cmd.args(repo), ctx).await?;
        }
        if !aur.is_empty() {
            let mut cmd = self.cmd().arg("-A");
            if ctx.assume_yes {
                cmd = cmd.arg("--noconfirm");
            }
            self.run(cmd.args(aur), ctx).await?;
        }
        Ok(())
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        // Repo updates only: AUR staleness is only visible through `-Au`.
        let output = self
            .query()
            .arg("-Qu")
            .capture(&self.elevator, None)
            .await?;
        // Exits non-zero with no output when everything is current.
        if !output.success() && output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(parse_outdated(&output.stdout))
    }
}

/// `-Q`: one `name version` per line.
fn parse_installed(stdout: &str) -> Vec<Box<dyn Package>> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            Some(Box::new(AuraPackage {
                name: name.to_string(),
                version: parts.next().map(str::to_string),
                state: InstallState::Installed,
                ..Default::default()
            }) as Box<dyn Package>)
        })
        .collect()
}

/// `-Ss`/`-As`: `repo/name version [extras]` headers with indented
/// descriptions.
fn parse_search(stdout: &str) -> Vec<Box<dyn Package>> {
    let mut packages: Vec<AuraPackage> = Vec::new();
    for line in stdout.lines() {
        if line.starts_with(char::is_whitespace) {
            if let (Some(last), text) = (packages.last_mut(), line.trim())
                && !text.is_empty()
            {
                match &mut last.description {
                    Some(description) => {
                        description.push(' ');
                        description.push_str(text);
                    }
                    None => last.description = Some(text.to_string()),
                }
            }
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some((origin, name)) = parts.next().and_then(|first| first.split_once('/')) else {
            continue;
        };
        packages.push(AuraPackage {
            name: name.to_string(),
            version: parts.next().map(str::to_string),
            origin: Some(origin.to_string()),
            state: if line.contains("[installed") {
                InstallState::Installed
            } else {
                InstallState::Available
            },
            ..Default::default()
        });
    }
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `-Qu`: `name current -> latest` per line.
fn parse_outdated(stdout: &str) -> Vec<Box<dyn Package>> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            let current = parts.next()?;
            if parts.next()? != "->" {
                return None;
            }
            Some(Box::new(AuraPackage {
                name: name.to_string(),
                version: Some(current.to_string()),
                latest_version: Some(parts.next()?.to_string()),
                state: InstallState::Upgradable,
                ..Default::default()
            }) as Box<dyn Package>)
        })
        .collect()
}

/// `-Qi`/`-Si`/`-Ai`: `Key : Value` fields; continuation lines are indented.
fn parse_info(stdout: &str, state: InstallState) -> Option<AuraPackage> {
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in stdout.lines() {
        if line.starts_with(char::is_whitespace) || !line.contains(':') {
            if let Some((_, value)) = fields.last_mut() {
                let text = line.trim();
                if !text.is_empty() {
                    value.push(' ');
                    value.push_str(text);
                }
            }
            continue;
        }
        let (key, value) = line.split_once(':')?;
        fields.push((key.trim().to_string(), value.trim().to_string()));
    }
    let mut package = AuraPackage {
        state,
        ..Default::default()
    };
    for (key, value) in fields {
        if value.is_empty() || value == "None" {
            continue;
        }
        match key.as_str() {
            "Name" => package.name = value,
            "Version" => package.version = Some(value),
            "Description" => package.description = Some(value),
            "URL" => package.homepage = Some(value),
            "Licenses" | "License" => package.license = Some(value),
            "Architecture" => package.architecture = Some(value),
            "Repository" => package.origin = Some(value),
            "Depends On" | "Depends" => {
                package.dependencies = Some(value.split_whitespace().map(str::to_string).collect());
            }
            _ => {}
        }
    }
    (!package.name.is_empty()).then_some(package)
}

/// A package as aura describes it.
#[derive(Debug, Default)]
pub struct AuraPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub architecture: Option<String>,
    pub origin: Option<String>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for AuraPackage {
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
    fn parses_installed_lines() {
        let packages = parse_installed("bash 5.2.026-2\naura-bin 4.0.0-1\n");
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name(), "aura-bin");
        assert_eq!(packages[1].state(), InstallState::Installed);
    }

    #[test]
    fn parses_search_from_both_sources() {
        let stdout = "\
aur/ripgrep-git 14.1.0.r13.g6f4212a-1 (+31 0.24)
    A search tool that combines the usability of ag with the raw speed of grep
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name(), "ripgrep-git");
        assert_eq!(packages[0].origin(), Some("aur"));
    }

    #[test]
    fn parses_outdated_lines() {
        let packages = parse_outdated("linux 6.9.1.arch1-1 -> 6.9.2.arch1-1\n");
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].state(), InstallState::Upgradable);
    }

    #[test]
    fn parses_info_fields() {
        let stdout = "\
Repository      : aur
Name            : ripgrep-git
Version         : 14.1.0.r13.g6f4212a-1
Description     : A search tool
URL             : https://github.com/BurntSushi/ripgrep
";
        let package = parse_info(stdout, InstallState::Available).unwrap();
        assert_eq!(package.name, "ripgrep-git");
        assert_eq!(package.origin.as_deref(), Some("aur"));
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("ripgrep@14.1.0")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("ripgrep")]).is_ok());
    }
}
