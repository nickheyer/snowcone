//! DNF backend for snowcone.
//!
//! Discovery prefers `dnf5` and falls back to dnf4, so every verb and flag
//! here sticks to the subset both major versions accept. `check-update`
//! exits 100 to mean "updates exist" - a result, not a failure. Neither
//! version has a dry-run flag shared with the other, so mutations error
//! under `dry_run`; `-y` covers `assume_yes`. Reads are parsed under
//! `LC_ALL=C` because both versions localize their output, and the listing
//! read goes through `repoquery --installed` with an explicit
//! `--queryformat` instead of the column-wrapped `list` tables.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "dnf";
const PROGRAMS: &[&str] = &["dnf5", "dnf"];
/// One `name<TAB>version-release<TAB>arch<TAB>summary` record per line; the
/// trailing newline is explicit because dnf5 does not add one per record.
const QUERYFORMAT: &str = "%{name}\t%{version}-%{release}\t%{arch}\t%{summary}\n";

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
                reason: format!("none of {PROGRAMS:?} found on PATH"),
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
    /// and an error on `dry_run` - there is no simulate flag valid on both
    /// dnf4 and dnf5.
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Result<Cmd> {
        if ctx.dry_run {
            return Err(Error::Other(format!(
                "{ID}: {subcommand} has no dry-run mode shared by dnf4 and dnf5"
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
        "DNF"
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
        let cmd = self.mutation("install", ctx)?.args(packages.iter().map(spec));
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
            .args(["repoquery", "--installed", "--queryformat", QUERYFORMAT])
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
        // Both versions exit non-zero on "no matches"; empty output is that,
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
        self.run(
            self.cmd().args(["makecache", "--refresh"]).elevated(true),
            ctx,
        )
        .await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = self.mutation("upgrade", ctx)?.args(packages.iter().map(spec));
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

fn boxed(packages: Vec<DnfPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `dnf repoquery --installed --queryformat`: tab-separated `name`,
/// `version-release`, `arch`, `summary` records (dnf4 appends its own
/// newline on top of the format's, leaving blank lines to skip).
fn parse_installed(stdout: &str) -> Vec<DnfPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?;
            let version = fields.next()?;
            let arch = fields.next()?;
            let summary = fields.next()?;
            if name.is_empty() {
                return None;
            }
            Some(DnfPackage {
                name: name.to_string(),
                version: Some(version.to_string()),
                architecture: Some(arch.to_string()),
                description: Some(summary.to_string()).filter(|summary| !summary.is_empty()),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `dnf search`: `name.arch : summary` result lines (dnf5 drops the space
/// before the colon); banner and metadata lines carry whitespace before
/// their first colon, which is what filters them out.
fn parse_search(stdout: &str) -> Vec<DnfPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let (left, summary) = line.split_once(':')?;
            let left = left.trim();
            if left.is_empty() || left.contains(char::is_whitespace) {
                return None;
            }
            let (name, arch) = left.rsplit_once('.')?;
            Some(DnfPackage {
                name: name.to_string(),
                architecture: Some(arch.to_string()),
                description: Some(summary.trim().to_string()).filter(|s| !s.is_empty()),
                state: InstallState::Available,
                ..Default::default()
            })
        })
        .collect()
}

/// `dnf check-update`: `name.arch  version  repo` rows; long names wrap onto
/// their own line with the rest indented on the next, and everything under
/// "Obsoleting Packages" is a different report.
fn parse_updates(stdout: &str) -> Vec<DnfPackage> {
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
                packages.push(DnfPackage {
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

/// `dnf info`: `Key : Value` stanzas under "Installed Packages"/"Available
/// Packages" section headers (dnf5 lowercases "packages"); wrapped values
/// continue on lines whose key side is empty.
fn parse_info(stdout: &str) -> Vec<DnfPackage> {
    fn finish(
        current: &mut Option<(DnfPackage, Option<String>, Option<String>)>,
        packages: &mut Vec<DnfPackage>,
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
    let mut current: Option<(DnfPackage, Option<String>, Option<String>)> = None;
    let mut last_key = String::new();
    for line in stdout.lines() {
        let lower = line.trim().to_ascii_lowercase();
        if lower.starts_with("installed package") {
            state = InstallState::Installed;
            continue;
        }
        if lower.starts_with("available package") {
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
                DnfPackage {
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
            "Architecture" | "Arch" => package.architecture = Some(value.to_string()),
            "Summary" => package.description = Some(value.to_string()),
            "URL" => package.homepage = Some(value.to_string()),
            "License" => package.license = Some(value.to_string()),
            // "@System" only says "installed", which the section already did.
            "Repository" if value != "@System" => package.origin = Some(value.to_string()),
            "From repo" | "From repository" => package.origin = Some(value.to_string()),
            _ => {}
        }
    }
    finish(&mut current, &mut packages);
    packages
}

/// A package as dnf describes it.
#[derive(Debug, Default)]
pub struct DnfPackage {
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

impl Package for DnfPackage {
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
    fn parses_repoquery_records() {
        let stdout = "\
bash\t5.2.26-3.fc40\tx86_64\tThe GNU Bourne Again shell

ripgrep\t14.1.0-1.fc40\tx86_64\tLine-oriented search tool
";
        let packages = parse_installed(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "ripgrep");
        assert_eq!(packages[1].version.as_deref(), Some("14.1.0-1.fc40"));
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn parses_dnf4_search_output() {
        let stdout = "\
Last metadata expiration check: 0:23:15 ago on Sun Aug  9 10:12:00 2026.
========================== Name Exactly Matched: ripgrep ==========================
ripgrep.x86_64 : Line-oriented search tool using Rust's regex library
=========================== Summary Matched: search ===============================
fd-find.x86_64 : Simple, fast alternative to find
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "ripgrep");
        assert_eq!(packages[0].architecture.as_deref(), Some("x86_64"));
        assert_eq!(
            packages[0].description.as_deref(),
            Some("Line-oriented search tool using Rust's regex library")
        );
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn parses_dnf5_search_output() {
        let stdout = "\
Updating and loading repositories:
Repositories loaded.
Matched fields: name, summary
 ripgrep.x86_64: Line-oriented search tool using Rust's regex library
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "ripgrep");
    }

    #[test]
    fn parses_check_update_with_wrapped_names() {
        let stdout = "\
Last metadata expiration check: 0:11:22 ago on Sun Aug  9 10:12:00 2026.

ripgrep.x86_64                          14.1.0-2.fc40                    updates
container-selinux.noarch
                                        2:2.232.1-1.fc40                 updates
Obsoleting Packages
grub2-tools.x86_64                      1:2.06-123.fc40                  updates
";
        let packages = parse_updates(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "ripgrep");
        assert_eq!(packages[0].latest_version.as_deref(), Some("14.1.0-2.fc40"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
        assert_eq!(packages[1].name, "container-selinux");
        assert_eq!(
            packages[1].latest_version.as_deref(),
            Some("2:2.232.1-1.fc40")
        );
    }

    #[test]
    fn parses_info_stanzas_with_states() {
        let stdout = "\
Last metadata expiration check: 0:01:02 ago on Sun Aug  9 10:12:00 2026.
Installed Packages
Name         : ripgrep
Version      : 14.1.0
Release      : 1.fc40
Architecture : x86_64
Size         : 4.2 M
Source       : ripgrep-14.1.0-1.fc40.src.rpm
Repository   : @System
From repo    : updates
Summary      : Line-oriented search tool using Rust's regex
             : library
URL          : https://github.com/BurntSushi/ripgrep
License      : MIT OR Unlicense
Description  : ripgrep is a line-oriented search tool.
Available Packages
Name         : ripgrep
Version      : 14.1.0
Release      : 2.fc40
Architecture : x86_64
Repository   : updates
Summary      : Line-oriented search tool using Rust's regex library
";
        let stanzas = parse_info(stdout);
        assert_eq!(stanzas.len(), 2);
        assert_eq!(stanzas[0].state, InstallState::Installed);
        assert_eq!(stanzas[0].version.as_deref(), Some("14.1.0-1.fc40"));
        assert_eq!(stanzas[0].origin.as_deref(), Some("updates"));
        assert_eq!(
            stanzas[0].description.as_deref(),
            Some("Line-oriented search tool using Rust's regex library")
        );
        assert_eq!(stanzas[1].state, InstallState::Available);
        assert_eq!(stanzas[1].version.as_deref(), Some("14.1.0-2.fc40"));
    }

    #[test]
    fn parses_dnf5_info_headers() {
        let stdout = "\
Installed packages
Name           : ripgrep
Version        : 14.1.0
Release        : 1.fc40
Architecture   : x86_64
";
        let stanzas = parse_info(stdout);
        assert_eq!(stanzas.len(), 1);
        assert_eq!(stanzas[0].state, InstallState::Installed);
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("ripgrep@14.1.0-1.fc40")),
            "ripgrep-14.1.0-1.fc40"
        );
        assert_eq!(spec(&PackageRequest::parse("ripgrep")), "ripgrep");
    }
}
