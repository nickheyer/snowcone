//! YUM backend for snowcone.
//!
//! Targets the classic yum of RHEL 7 / Amazon Linux 2 era systems, so only
//! the long-standing verbs are used (`install`, `remove`, `update`,
//! `search`, `info`, `list installed`, `check-update`, `makecache`).
//! `yum list` wraps long package names onto their own line with the
//! remaining columns indented beneath - the parser reassembles those rows.
//! `check-update` exits 100 to mean "updates exist". yum has no dry-run
//! flag, so mutations error under `dry_run`; `-y` covers `assume_yes`, and
//! every parsed read runs under `LC_ALL=C`.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "yum";
const PROGRAMS: &[&str] = &["yum"];

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

    /// Shared shape for mutating commands: elevated, `-y` on `assume_yes`,
    /// and an error on `dry_run` - yum has no simulate flag.
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Result<Cmd> {
        if ctx.dry_run {
            return Err(Error::Other(format!(
                "{ID}: {subcommand} has no dry-run mode"
            )));
        }
        let mut cmd = self.cmd().arg(subcommand).elevated(true);
        if ctx.assume_yes {
            cmd = cmd.arg("-y");
        }
        Ok(cmd)
    }
}

/// `name-version` when the request pins one, bare name otherwise.
fn spec(request: &PackageRequest) -> String {
    match &request.version {
        Some(version) => format!("{}-{version}", request.name),
        None => request.name.clone(),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "YUM"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "rpmdb"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::REFRESH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
            | Capabilities::PIN_VERSION
    }

    fn needs_elevation(&self, operation: Operation) -> bool {
        matches!(
            operation,
            Operation::Install | Operation::Remove | Operation::Upgrade | Operation::Refresh
        )
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = self
            .mutation("install", ctx)?
            .args(packages.iter().map(spec));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = self
            .mutation("remove", ctx)?
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["list", "installed"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_installed(&output.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query()
            .args(["info", name])
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        let mut stanzas = parse_info(&output.stdout);
        let installed = stanzas
            .iter()
            .position(|stanza| stanza.state == InstallState::Installed)
            .map(|index| stanzas.remove(index));
        let package = match installed {
            Some(mut package) => {
                // An available stanza with a different version is the update
                // candidate.
                if let Some(available) = stanzas.iter().find(|stanza| {
                    stanza.state == InstallState::Available
                        && stanza.version.is_some()
                        && stanza.version != package.version
                }) {
                    package.latest_version = available.version.clone();
                    package.state = InstallState::Upgradable;
                }
                package
            }
            None => match stanzas.into_iter().next() {
                Some(package) => package,
                None => return Err(Error::NotFound(name.to_string())),
            },
        };
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["search", query])
            .capture(&self.elevator, None)
            .await?;
        // yum exits non-zero on "No matches found"; that is an empty result,
        // not a failure.
        if !output.success() && output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(boxed(parse_search(&output.stdout)))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: refresh has no dry-run mode")));
        }
        self.run(self.cmd().arg("makecache").elevated(true), ctx)
            .await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = self
            .mutation("update", ctx)?
            .args(packages.iter().map(spec));
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("check-update")
            .capture(&self.elevator, None)
            .await?;
        // 100 means "updates exist"; 0 means everything is current.
        if output.status.code() == Some(100) {
            return Ok(boxed(parse_updates(&output.stdout)));
        }
        output.require_success()?;
        Ok(Vec::new())
    }
}

fn boxed(packages: Vec<YumPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `yum list installed`: `name.arch  [epoch:]version-release  @repo` rows,
/// where a long name wraps onto its own line and the remaining two columns
/// follow indented on the next.
fn parse_installed(stdout: &str) -> Vec<YumPackage> {
    let mut packages = Vec::new();
    let mut pending: Option<String> = None;
    for raw in stdout.lines() {
        let line = match pending.take() {
            Some(name) => format!("{name} {raw}"),
            None => raw.to_string(),
        };
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields.as_slice() {
            [name] if name.contains('.') => pending = Some(name.to_string()),
            [name, version, repo] if name.contains('.') => {
                let Some((name, arch)) = name.rsplit_once('.') else {
                    continue;
                };
                packages.push(YumPackage {
                    name: name.to_string(),
                    version: Some(version.to_string()),
                    architecture: Some(arch.to_string()),
                    // '@' marks "installed from"; the repo name follows it.
                    origin: Some(repo.strip_prefix('@').unwrap_or(repo).to_string()),
                    state: InstallState::Installed,
                    ..Default::default()
                });
            }
            _ => {}
        }
    }
    packages
}

/// `yum search`: `name.arch : summary` result lines; banner and plugin
/// chatter carries whitespace before its first colon, which filters it out.
fn parse_search(stdout: &str) -> Vec<YumPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let (left, summary) = line.split_once(':')?;
            let left = left.trim();
            if left.is_empty() || left.contains(char::is_whitespace) {
                return None;
            }
            let (name, arch) = left.rsplit_once('.')?;
            Some(YumPackage {
                name: name.to_string(),
                architecture: Some(arch.to_string()),
                description: Some(summary.trim().to_string()).filter(|s| !s.is_empty()),
                state: InstallState::Available,
                ..Default::default()
            })
        })
        .collect()
}

/// `yum check-update`: `name.arch  version  repo` rows with the same
/// name-wrapping as `yum list`; everything under "Obsoleting Packages" is a
/// different report.
fn parse_updates(stdout: &str) -> Vec<YumPackage> {
    let mut packages = Vec::new();
    let mut pending: Option<String> = None;
    for raw in stdout.lines() {
        if raw.trim_start().starts_with("Obsoleting") {
            break;
        }
        let line = match pending.take() {
            Some(name) => format!("{name} {raw}"),
            None => raw.to_string(),
        };
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields.as_slice() {
            [name] if name.contains('.') => pending = Some(name.to_string()),
            [name, version, repo] if name.contains('.') => {
                let Some((name, arch)) = name.rsplit_once('.') else {
                    continue;
                };
                packages.push(YumPackage {
                    name: name.to_string(),
                    architecture: Some(arch.to_string()),
                    latest_version: Some(version.to_string()),
                    origin: Some(repo.to_string()),
                    state: InstallState::Upgradable,
                    ..Default::default()
                });
            }
            _ => {}
        }
    }
    packages
}

/// `yum info`: `Key : Value` stanzas under "Installed Packages"/"Available
/// Packages" section headers; wrapped values continue on lines whose key
/// side is empty.
fn parse_info(stdout: &str) -> Vec<YumPackage> {
    fn finish(
        current: &mut Option<(YumPackage, Option<String>, Option<String>)>,
        packages: &mut Vec<YumPackage>,
    ) {
        if let Some((mut package, version, release)) = current.take() {
            package.version = match (version, release) {
                (Some(version), Some(release)) => Some(format!("{version}-{release}")),
                (version, None) => version,
                (None, _) => None,
            };
            if !package.name.is_empty() {
                packages.push(package);
            }
        }
    }

    let mut packages = Vec::new();
    let mut state = InstallState::Unknown;
    let mut current: Option<(YumPackage, Option<String>, Option<String>)> = None;
    let mut last_key = String::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed == "Installed Packages" {
            state = InstallState::Installed;
            continue;
        }
        if trimmed == "Available Packages" || trimmed == "Updated Packages" {
            state = InstallState::Available;
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if key.is_empty() {
            // Continuation of the previous field; only the summary matters.
            if last_key == "Summary"
                && !value.is_empty()
                && let Some((package, _, _)) = current.as_mut()
                && let Some(description) = package.description.as_mut()
            {
                description.push(' ');
                description.push_str(value);
            }
            continue;
        }
        last_key = key.to_string();
        if key == "Name" {
            finish(&mut current, &mut packages);
            current = Some((
                YumPackage {
                    name: value.to_string(),
                    state,
                    ..Default::default()
                },
                None,
                None,
            ));
            continue;
        }
        let Some((package, version, release)) = current.as_mut() else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        match key {
            "Version" => *version = Some(value.to_string()),
            "Release" => *release = Some(value.to_string()),
            "Arch" => package.architecture = Some(value.to_string()),
            "Summary" => package.description = Some(value.to_string()),
            "URL" => package.homepage = Some(value.to_string()),
            "License" => package.license = Some(value.to_string()),
            // "installed" only restates the section; "From repo" names the
            // real origin for installed packages.
            "Repo" if value != "installed" => package.origin = Some(value.to_string()),
            "From repo" => package.origin = Some(value.to_string()),
            _ => {}
        }
    }
    finish(&mut current, &mut packages);
    packages
}

/// A package as yum describes it.
#[derive(Debug, Default)]
pub struct YumPackage {
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

impl Package for YumPackage {
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
    fn parses_installed_list_with_wrapped_names() {
        let stdout = "\
Loaded plugins: fastestmirror, langpacks
Installed Packages
NetworkManager.x86_64                 1:1.18.8-2.el7_9                @updates
basesystem.noarch                     10.0-7.el7.centos               @anaconda
java-1.8.0-openjdk-headless.x86_64
                                      1:1.8.0.412.b08-1.el7_9         @updates
";
        let packages = parse_installed(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "NetworkManager");
        assert_eq!(packages[0].version.as_deref(), Some("1:1.18.8-2.el7_9"));
        assert_eq!(packages[0].origin.as_deref(), Some("updates"));
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[2].name, "java-1.8.0-openjdk-headless");
        assert_eq!(packages[2].architecture.as_deref(), Some("x86_64"));
        assert_eq!(
            packages[2].version.as_deref(),
            Some("1:1.8.0.412.b08-1.el7_9")
        );
    }

    #[test]
    fn parses_search_entries() {
        let stdout = "\
Loaded plugins: fastestmirror, langpacks
============================== N/S matched: ripgrep ===============================
ripgrep.x86_64 : Line-oriented search tool using Rust's regex library

  Name and summary matches only, use \"search all\" for everything.
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "ripgrep");
        assert_eq!(
            packages[0].description.as_deref(),
            Some("Line-oriented search tool using Rust's regex library")
        );
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn parses_check_update_with_wrapped_names() {
        let stdout = "\
Loaded plugins: fastestmirror, langpacks

kernel.x86_64                           3.10.0-1160.119.1.el7            updates
java-1.8.0-openjdk-headless.x86_64
                                        1:1.8.0.422.b05-1.el7_9          updates
Obsoleting Packages
grub2.x86_64                            1:2.02-0.87.el7.centos           updates
";
        let packages = parse_updates(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "kernel");
        assert_eq!(
            packages[0].latest_version.as_deref(),
            Some("3.10.0-1160.119.1.el7")
        );
        assert_eq!(packages[0].state, InstallState::Upgradable);
        assert_eq!(packages[1].name, "java-1.8.0-openjdk-headless");
    }

    #[test]
    fn parses_info_stanzas_with_states() {
        let stdout = "\
Loaded plugins: fastestmirror, langpacks
Installed Packages
Name        : ripgrep
Arch        : x86_64
Version     : 14.1.0
Release     : 1.el7
Size        : 8.0 M
Repo        : installed
From repo   : epel
Summary     : Line-oriented search tool using Rust's regex
            : library
URL         : https://github.com/BurntSushi/ripgrep
License     : MIT
Description : ripgrep is a line-oriented search tool.

Available Packages
Name        : ripgrep
Arch        : x86_64
Version     : 14.1.0
Release     : 2.el7
Repo        : epel
Summary     : Line-oriented search tool using Rust's regex library
";
        let stanzas = parse_info(stdout);
        assert_eq!(stanzas.len(), 2);
        assert_eq!(stanzas[0].state, InstallState::Installed);
        assert_eq!(stanzas[0].version.as_deref(), Some("14.1.0-1.el7"));
        assert_eq!(stanzas[0].origin.as_deref(), Some("epel"));
        assert_eq!(
            stanzas[0].description.as_deref(),
            Some("Line-oriented search tool using Rust's regex library")
        );
        assert_eq!(stanzas[1].state, InstallState::Available);
        assert_eq!(stanzas[1].version.as_deref(), Some("14.1.0-2.el7"));
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("ripgrep@14.1.0-1.el7")),
            "ripgrep-14.1.0-1.el7"
        );
        assert_eq!(spec(&PackageRequest::parse("ripgrep")), "ripgrep");
    }
}
