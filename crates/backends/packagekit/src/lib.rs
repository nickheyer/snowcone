//! PackageKit backend for snowcone.
//!
//! Drives `pkcon`, PackageKit's distro-neutral console client. The daemon
//! authorizes mutations itself through polkit, so nothing here is elevated -
//! polkit prompts through its own agent. Queries run with `--plain` under
//! `LC_ALL=C`: result rows are tab-separated (`State`, `name-version.arch`,
//! summary) after a stream of `Status:`-style progress rows. pkcon has no
//! simulate switch, so every mutation refuses `--dry-run`; "nothing found"
//! and "no updates" outcomes exit non-zero and are told apart from real
//! failures by pkcon's complaint text.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, CmdOutput, Detection, Elevator, Error, HostInfo,
    InstallState, ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest,
    Result, find_program,
};

const ID: &str = "packagekit";
const PROGRAMS: &[&str] = &["pkcon"];

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

    /// Read invocation: `--plain` swaps the animated widgets for parseable
    /// rows, `LC_ALL=C` keeps them English.
    fn query(&self) -> Cmd {
        Cmd::new(&self.program).arg("--plain").env("LC_ALL", "C")
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

    /// Mutating command shape: never elevated (the daemon self-authorizes
    /// via polkit), `-y` when prompts should be answered.
    fn mutation(&self, verb: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(verb);
        if ctx.assume_yes {
            cmd = cmd.arg("-y");
        }
        cmd
    }

    fn no_dry_run(&self, operation: &str) -> Error {
        Error::Other(format!("{ID}: {operation} has no dry-run mode"))
    }
}

/// pkcon reports "nothing to do" outcomes as failures; its complaint text
/// (stdout or stderr depending on version) tells them from real errors.
fn mentions(output: &CmdOutput, needles: &[&str]) -> bool {
    needles
        .iter()
        .any(|needle| output.stdout.contains(needle) || output.stderr.contains(needle))
}

/// PackageKit resolves names to the newest available version; there is no
/// version selection in pkcon.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but pkcon only installs the newest available package"
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
        "PackageKit"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Other
    }

    fn database_id(&self) -> &'static str {
        "packagekit"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::REFRESH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
    }

    /// snowcone never prefixes sudo here, but the daemon authorizes every
    /// mutation through polkit - a credential prompt is still coming, and
    /// callers plan for it off this flag.
    fn needs_elevation(&self, operation: Operation) -> bool {
        operation.mutates()
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        let cmd = self
            .mutation("install", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        let cmd = self
            .mutation("remove", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["get-packages", "--filter", "installed"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_packages(&output.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let details = self
            .query()
            .arg("get-details")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !details.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        let mut package =
            parse_details(&details.stdout).ok_or_else(|| Error::NotFound(name.to_string()))?;
        // `get-details` describes the repository side; one `resolve` probe
        // says whether the package is installed locally, and at what version.
        let resolve = self
            .query()
            .arg("resolve")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if let Some(installed) = parse_packages(&resolve.stdout)
            .into_iter()
            .find(|candidate| {
                candidate.name == package.name && candidate.state == InstallState::Installed
            })
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
            .args(["search", "name"])
            .arg(query)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() && mentions(&output, &["no packages were found", "could not find"]) {
            return Ok(Vec::new());
        }
        let output = output.require_success()?;
        Ok(boxed(parse_packages(&output.stdout)))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("refresh"));
        }
        self.run(self.cmd().arg("refresh"), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        let cmd = self
            .mutation("update", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("get-updates")
            .capture(&self.elevator, None)
            .await?;
        if !output.success() && mentions(&output, &["no updates"]) {
            return Ok(Vec::new());
        }
        let output = output.require_success()?;
        Ok(boxed(parse_updates(&output.stdout)))
    }
}

fn boxed(packages: Vec<PackagekitPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// pkcon's printable package id `name-version.arch`: the arch is the segment
/// after the last dot (pkcon always appends one) and the version starts at
/// the first dash followed by a digit - best effort, since names and
/// versions may themselves contain dashes.
fn split_display(display: &str) -> (String, Option<String>, Option<String>) {
    let (rest, architecture) = match display.rsplit_once('.') {
        Some((rest, arch)) if !rest.is_empty() && !arch.is_empty() => {
            (rest, Some(arch.to_string()))
        }
        _ => (display, None),
    };
    for (idx, _) in rest.match_indices('-') {
        if rest[idx + 1..].starts_with(|c: char| c.is_ascii_digit()) {
            return (
                rest[..idx].to_string(),
                Some(rest[idx + 1..].to_string()),
                architecture,
            );
        }
    }
    (rest.to_string(), None, architecture)
}

/// One `pkcon --plain` result row: tab-separated state, `name-version.arch`,
/// summary, the first column space-padded. Progress rows (`Status:`,
/// `Percentage:`, the `Results:` banner) end their first field with `:` and
/// rows without tabs are widget noise - both are skipped.
fn parse_row(line: &str) -> Option<(String, PackagekitPackage)> {
    let mut fields = line.split('\t').map(str::trim);
    let state = fields.next()?;
    if state.is_empty() || state.ends_with(':') {
        return None;
    }
    let display = fields.next()?;
    if display.is_empty() {
        return None;
    }
    let (name, version, architecture) = split_display(display);
    if name.is_empty() {
        return None;
    }
    Some((
        state.to_string(),
        PackagekitPackage {
            name,
            version,
            architecture,
            description: fields.next().filter(|s| !s.is_empty()).map(str::to_string),
            ..Default::default()
        },
    ))
}

/// `pkcon get-packages`/`search`/`resolve` rows: only `Installed` and
/// `Available` rows are packages, everything else is transaction noise.
fn parse_packages(stdout: &str) -> Vec<PackagekitPackage> {
    stdout
        .lines()
        .filter_map(parse_row)
        .filter_map(|(state, mut package)| {
            package.state = match state.as_str() {
                "Installed" => InstallState::Installed,
                "Available" => InstallState::Available,
                _ => return None,
            };
            Some(package)
        })
        .collect()
}

/// `pkcon get-updates` rows: the first column is the update severity
/// (`Security`, `Bug fix`, …) and the row names the *new* version, so it
/// lands in `latest_version`; the installed version is not reported.
fn parse_updates(stdout: &str) -> Vec<PackagekitPackage> {
    stdout
        .lines()
        .filter_map(parse_row)
        .map(|(_, mut package)| {
            package.latest_version = package.version.take();
            package.state = InstallState::Upgradable;
            package
        })
        .collect()
}

/// `pkcon get-details`: indented lowercase `key: value` rows under a
/// `Details:` banner; the description value continues on unprefixed lines
/// until the next field. A URL at line start is not a field - real fields
/// have whitespace (or nothing) after the colon.
fn parse_details(stdout: &str) -> Option<PackagekitPackage> {
    let mut package = PackagekitPackage {
        state: InstallState::Available,
        ..Default::default()
    };
    let mut in_description = false;
    for line in stdout.lines() {
        let trimmed = line.trim();
        let field = trimmed.split_once(':').and_then(|(key, value)| {
            let key_ok = !key.is_empty() && key.chars().all(|c| c.is_ascii_lowercase());
            let value_ok = value.is_empty() || value.starts_with(char::is_whitespace);
            (key_ok && value_ok).then(|| (key, value.trim()))
        });
        match field {
            Some((key, value)) => {
                in_description = false;
                if value.is_empty() {
                    continue;
                }
                match key {
                    "package" => {
                        let (name, version, architecture) = split_display(value);
                        package.name = name;
                        package.version = version;
                        package.architecture = architecture;
                    }
                    "license" => package.license = Some(value.to_string()),
                    "url" => package.homepage = Some(value.to_string()),
                    "size" => {
                        package.size = value
                            .split_whitespace()
                            .next()
                            .and_then(|bytes| bytes.parse().ok());
                    }
                    "description" => {
                        package.description = Some(value.to_string());
                        in_description = true;
                    }
                    _ => {}
                }
            }
            None if in_description && !trimmed.is_empty() => match &mut package.description {
                Some(description) => {
                    description.push(' ');
                    description.push_str(trimmed);
                }
                None => package.description = Some(trimmed.to_string()),
            },
            None => {}
        }
    }
    (!package.name.is_empty()).then_some(package)
}

/// A package as pkcon describes it.
#[derive(Debug, Default)]
pub struct PackagekitPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub homepage: Option<String>,
    pub architecture: Option<String>,
    /// PackageKit reports one size: the installed size for installed
    /// packages, the download size otherwise - exposed accordingly.
    pub size: Option<u64>,
    pub state: InstallState,
}

impl Package for PackagekitPackage {
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

    fn installed_size(&self) -> Option<u64> {
        match self.state {
            InstallState::Installed | InstallState::Upgradable => self.size,
            _ => None,
        }
    }

    fn download_size(&self) -> Option<u64> {
        match self.state {
            InstallState::Available | InstallState::Unknown => self.size,
            _ => None,
        }
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_printable_package_ids() {
        assert_eq!(
            split_display("bash-5.2.26-3.fc40.x86_64"),
            (
                "bash".to_string(),
                Some("5.2.26-3.fc40".to_string()),
                Some("x86_64".to_string())
            )
        );
        assert_eq!(
            split_display("python3-libs-3.6.8-51.el8.x86_64"),
            (
                "python3-libs".to_string(),
                Some("3.6.8-51.el8".to_string()),
                Some("x86_64".to_string())
            )
        );
    }

    #[test]
    fn parses_package_rows_and_skips_progress() {
        let stdout = "\
Transaction:\tGetting packages
Status: \tWaiting in queue
Status: \tLoading cache
Percentage:\t100
Status: \tFinished
Results:
Installed   \tbash-5.2.26-3.fc40.x86_64\tThe GNU Bourne Again shell
Available   \tzsh-5.9-16.fc40.x86_64\tPowerful interactive shell
";
        let packages = parse_packages(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "bash");
        assert_eq!(packages[0].version.as_deref(), Some("5.2.26-3.fc40"));
        assert_eq!(packages[0].architecture.as_deref(), Some("x86_64"));
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(
            packages[1].description.as_deref(),
            Some("Powerful interactive shell")
        );
        assert_eq!(packages[1].state, InstallState::Available);
    }

    #[test]
    fn parses_update_rows_with_multiword_severities() {
        let stdout = "\
Transaction:\tGetting updates
Status: \tFinished
Results:
Security    \tkernel-6.9.4-200.fc40.x86_64\tThe Linux kernel
Bug fix     \tvim-enhanced-9.1.452-1.fc40.x86_64\tA version of the VIM editor
";
        let updates = parse_updates(stdout);
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].name, "kernel");
        assert_eq!(updates[0].version, None);
        assert_eq!(updates[0].latest_version.as_deref(), Some("6.9.4-200.fc40"));
        assert_eq!(updates[1].name, "vim-enhanced");
        assert_eq!(updates[1].state, InstallState::Upgradable);
    }

    #[test]
    fn parses_details_with_multiline_description() {
        let stdout = "\
Transaction:\tGetting details
Status: \tFinished
Details:
 package:              firefox-127.0-1.fc40.x86_64
 updates:
 license:              MPLv1.1
 group:                unknown
 description:          Mozilla Firefox is an open-source web browser, designed for standards
compliance, performance and portability.
https://www.mozilla.org/firefox/ has more information.
 size:                 261412302 bytes
 url:                  https://www.mozilla.org/firefox/
";
        let package = parse_details(stdout).unwrap();
        assert_eq!(package.name, "firefox");
        assert_eq!(package.version.as_deref(), Some("127.0-1.fc40"));
        assert_eq!(package.architecture.as_deref(), Some("x86_64"));
        assert_eq!(package.license.as_deref(), Some("MPLv1.1"));
        assert_eq!(
            package.homepage.as_deref(),
            Some("https://www.mozilla.org/firefox/")
        );
        assert_eq!(package.size, Some(261_412_302));
        let description = package.description.as_deref().unwrap();
        assert!(description.starts_with("Mozilla Firefox is an open-source"));
        assert!(description.contains("compliance, performance and portability."));
        assert!(description.contains("https://www.mozilla.org/firefox/ has more information."));
        assert_eq!(package.state, InstallState::Available);
        assert_eq!(package.download_size(), Some(261_412_302));
        assert_eq!(package.installed_size(), None);
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("firefox@127.0")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("firefox")]).is_ok());
    }
}
