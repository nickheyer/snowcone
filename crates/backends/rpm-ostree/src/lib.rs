//! rpm-ostree backend for snowcone.
//!
//! The CLI talks to a privileged daemon that authorizes every request
//! through polkit, so snowcone never prefixes an elevation helper even
//! though mutations report `needs_elevation`. Mutations stage a new
//! deployment that only takes effect on the next boot. `status --json` is
//! the authoritative machine-readable state, and only layered/requested
//! packages are listed - the base image is managed as one unit, so
//! `upgrade` moves the whole image and the outdated listing is a single
//! image-level entry from `upgrade --check` (exit 77 means already
//! current). rpm-ostree never prompts, so `assume_yes` has nothing to do.
//! `info` reads the booted deployment's rpmdb through the host `rpm`
//! binary, which is always present on ostree systems.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "rpm-ostree";
const PROGRAMS: &[&str] = &["rpm-ostree"];

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
            rpm: find_program("rpm"),
            elevator: Elevator::detect(host),
        }))
    }
}

struct Manager {
    program: PathBuf,
    /// Host `rpm`, for querying the booted deployment's rpmdb.
    rpm: Option<PathBuf>,
    elevator: Elevator,
}

impl Manager {
    fn cmd(&self) -> Cmd {
        Cmd::new(&self.program)
    }

    /// Read invocation with a stable locale, so parsing survives i18n.
    fn query(&self) -> Cmd {
        Cmd::new(&self.program).env("LC_ALL", "C")
    }

    /// Read invocation of the host `rpm`, with a stable locale.
    fn rpm_query(&self) -> Result<Cmd> {
        let rpm = self.rpm.as_deref().ok_or_else(|| {
            Error::Other(format!("{ID}: `rpm` not found on PATH to read the rpmdb"))
        })?;
        Ok(Cmd::new(rpm).env("LC_ALL", "C"))
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

    /// The daemon's full state, from `status --json`.
    async fn status(&self) -> Result<Value> {
        let output = self
            .cmd()
            .args(["status", "--json"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        serde_json::from_str(&output.stdout).map_err(|error| Error::Parse {
            what: format!("{ID} status output"),
            detail: error.to_string(),
        })
    }
}

/// Layered installs track whatever the repos hold at deploy time; there is
/// no version selection.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but layered packages always track the repository"
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
        "rpm-ostree"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "rpmdb"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::REFRESH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
    }

    fn needs_elevation(&self, operation: Operation) -> bool {
        matches!(
            operation,
            Operation::Install | Operation::Remove | Operation::Upgrade | Operation::Refresh
        )
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        let mut cmd = self.cmd().arg("install");
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        cmd = cmd.args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let mut cmd = self.cmd().arg("uninstall");
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        cmd = cmd.args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let status = self.status().await?;
        Ok(parse_layered(&status)
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .rpm_query()?
            .arg("-qi")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        // `rpm -qi` exits non-zero for anything not in the rpmdb.
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        let package = parse_rpm_info(&output.stdout).ok_or_else(|| Error::Parse {
            what: format!("{ID} rpm -qi output"),
            detail: format!("no `Name` field for `{name}`"),
        })?;
        Ok(Box::new(package))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: refresh has no dry-run mode")));
        }
        self.run(self.cmd().arg("refresh-md"), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if !packages.is_empty() {
            return Err(Error::Other(format!(
                "{ID}: the deployment upgrades as one unit; per-package upgrade is not supported"
            )));
        }
        let mut cmd = self.cmd().arg("upgrade");
        if ctx.dry_run {
            // `--preview` downloads only the package diff between the
            // booted and pending image - the closest thing to a dry run.
            cmd = cmd.arg("--preview");
        }
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let check = self
            .query()
            .args(["upgrade", "--check"])
            .capture(&self.elevator, None)
            .await?;
        // `--check` exits 77 when the system is already current.
        if check.status.code() == Some(77) {
            return Ok(Vec::new());
        }
        let check = check.require_success()?;
        let latest = parse_check(&check.stdout);
        // The base image is the upgrade unit; one entry stands in for it,
        // named after the booted deployment's os.
        let status = self.status().await?;
        let booted = booted_deployment(&status);
        Ok(vec![Box::new(RpmOstreePackage {
            name: booted
                .and_then(|deployment| deployment["osname"].as_str())
                .unwrap_or("os")
                .to_string(),
            version: booted
                .and_then(|deployment| deployment["version"].as_str())
                .map(str::to_string),
            latest_version: latest,
            state: InstallState::Upgradable,
            ..Default::default()
        })])
    }
}

/// The deployment currently running, falling back to the first listed.
fn booted_deployment(status: &Value) -> Option<&Value> {
    let deployments = status["deployments"].as_array()?;
    deployments
        .iter()
        .find(|deployment| deployment["booted"].as_bool() == Some(true))
        .or_else(|| deployments.first())
}

/// `status --json`: the booted deployment's `packages` (layered) and
/// `requested-packages` (may include not-yet-active requests) name arrays,
/// deduplicated - versions are not part of the deployment state.
fn parse_layered(status: &Value) -> Vec<RpmOstreePackage> {
    let Some(deployment) = booted_deployment(status) else {
        return Vec::new();
    };
    let mut names: Vec<String> = Vec::new();
    for field in ["packages", "requested-packages"] {
        for name in deployment[field]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !names.iter().any(|seen| seen == name) {
                names.push(name.to_string());
            }
        }
    }
    names
        .into_iter()
        .map(|name| RpmOstreePackage {
            name,
            state: InstallState::Installed,
            ..Default::default()
        })
        .collect()
}

/// `upgrade --check`: an `AvailableUpdate:` block whose indented `Version:`
/// field carries the pending version, with a parenthesized timestamp
/// suffix.
fn parse_check(stdout: &str) -> Option<String> {
    let mut in_block = false;
    for line in stdout.lines() {
        if line.trim() == "AvailableUpdate:" {
            in_block = true;
            continue;
        }
        if in_block && let Some(version) = line.trim().strip_prefix("Version:") {
            let version = version.trim();
            let version = version.split_once(" (").map_or(version, |(v, _)| v);
            return Some(version.to_string());
        }
    }
    None
}

/// `rpm -qi`: `Key : Value` fields with a multi-line description at the
/// end; `Version` and `Release` join into the full version string, and
/// `Size` is plain bytes.
fn parse_rpm_info(stdout: &str) -> Option<RpmOstreePackage> {
    let mut package = RpmOstreePackage {
        state: InstallState::Installed,
        ..Default::default()
    };
    let mut version = None;
    let mut release = None;
    for line in stdout.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if value.is_empty() {
            continue;
        }
        match key {
            "Name" => package.name = value.to_string(),
            "Version" => version = Some(value.to_string()),
            "Release" => release = Some(value.to_string()),
            "Architecture" => package.architecture = Some(value.to_string()),
            "License" => package.license = Some(value.to_string()),
            "URL" => package.homepage = Some(value.to_string()),
            "Summary" => package.description = Some(value.to_string()),
            "Size" => package.installed_size = value.parse().ok(),
            _ => {}
        }
    }
    package.version = match (version, release) {
        (Some(version), Some(release)) => Some(format!("{version}-{release}")),
        (version, _) => version,
    };
    (!package.name.is_empty()).then_some(package)
}

/// A package as rpm-ostree (and the booted rpmdb) describes it.
#[derive(Debug, Default)]
pub struct RpmOstreePackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub architecture: Option<String>,
    pub installed_size: Option<u64>,
    pub state: InstallState,
}

impl Package for RpmOstreePackage {
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
        self.installed_size
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_layered_packages_from_booted_deployment() {
        let status: Value = serde_json::from_str(
            r#"{"deployments": [
                {"osname": "fedora", "version": "40.20240610.0", "booted": false,
                 "packages": ["htop"], "requested-packages": ["htop"]},
                {"osname": "fedora", "version": "40.20240601.0", "booted": true,
                 "packages": ["vim", "htop"],
                 "requested-packages": ["vim", "htop", "distrobox"]}
            ]}"#,
        )
        .unwrap();
        let packages = parse_layered(&status);
        let names: Vec<&str> = packages
            .iter()
            .map(|package| package.name.as_str())
            .collect();
        assert_eq!(names, ["vim", "htop", "distrobox"]);
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn empty_status_parses_to_nothing() {
        let status: Value = serde_json::from_str(r#"{"deployments": []}"#).unwrap();
        assert!(parse_layered(&status).is_empty());
    }

    #[test]
    fn parses_available_update_version() {
        let stdout = "\
AvailableUpdate:
        Version: 40.20240610.0 (2024-06-10T08:12:37Z)
        Commit: 3e4f8a9c1b
        Diff: 12 upgraded
";
        assert_eq!(parse_check(stdout).as_deref(), Some("40.20240610.0"));
    }

    #[test]
    fn no_update_block_parses_to_none() {
        assert_eq!(parse_check("No updates available.\n"), None);
    }

    #[test]
    fn parses_rpm_info_fields() {
        let stdout = "\
Name        : vim-enhanced
Version     : 9.1.158
Release     : 1.fc40
Architecture: x86_64
Install Date: Mon 10 Jun 2024 09:00:00 AM UTC
Size        : 4171922
License     : Vim AND MIT
URL         : http://www.vim.org/
Summary     : A version of the VIM editor which includes recent enhancements
Description :
VIM (VIsual editor iMproved) is an updated and improved version of the
vi editor.
";
        let package = parse_rpm_info(stdout).unwrap();
        assert_eq!(package.name, "vim-enhanced");
        assert_eq!(package.version.as_deref(), Some("9.1.158-1.fc40"));
        assert_eq!(package.architecture.as_deref(), Some("x86_64"));
        assert_eq!(package.license.as_deref(), Some("Vim AND MIT"));
        assert_eq!(package.homepage.as_deref(), Some("http://www.vim.org/"));
        assert_eq!(package.installed_size, Some(4171922));
        assert_eq!(
            package.description.as_deref(),
            Some("A version of the VIM editor which includes recent enhancements")
        );
        assert_eq!(package.state, InstallState::Installed);
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("vim@9.1.158")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("vim")]).is_ok());
    }
}
