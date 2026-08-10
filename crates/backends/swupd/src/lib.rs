//! swupd backend for snowcone.
//!
//! Clear Linux's updater manages *bundles*, not packages - every name this
//! backend accepts or returns is a bundle name. Bundles carry no version of
//! their own: the whole OS moves as one versioned stream, so `update`
//! upgrades everything at once and the outdated listing is a single
//! OS-level entry driven by `check-update`'s documented exit codes (0 means
//! an update exists, 1 means current). swupd runs non-interactively by
//! design; the documented global `--assume=yes` is passed on mutations to
//! also override warning prompts. Bundle search is delegated by swupd to
//! the swupd-search binary from the os-core-search bundle.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "swupd";
const PROGRAMS: &[&str] = &["swupd"];

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

    /// Shared shape for mutating commands: elevated, with the documented
    /// global yes-switch when asked for.
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(subcommand).elevated(true);
        if ctx.assume_yes {
            cmd = cmd.arg("--assume=yes");
        }
        cmd
    }

    fn no_dry_run(&self, operation: &str) -> Error {
        Error::Other(format!("{ID}: {operation} has no dry-run mode"))
    }
}

/// Bundles are unversioned - the OS updates as one stream, so a pinned
/// version has nothing to resolve against.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but swupd bundles are unversioned"
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
        "swupd"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "swupd"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
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
        if ctx.dry_run {
            return Err(self.no_dry_run("bundle-add"));
        }
        let cmd = self
            .mutation("bundle-add", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("bundle-remove"));
        }
        let cmd = self
            .mutation("bundle-remove", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("bundle-list")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_bundle_list(&output.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query()
            .arg("bundle-info")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        let package = parse_bundle_info(&output.stdout).ok_or_else(|| Error::Parse {
            what: format!("{ID} bundle-info output"),
            detail: format!("no `Info for bundle` header for `{name}`"),
        })?;
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("search")
            .arg(query)
            .capture(&self.elevator, None)
            .await?;
        // Tolerate a non-zero "no results" exit as long as output arrived.
        let output = if output.stdout.trim().is_empty() {
            output.require_success()?
        } else {
            output
        };
        Ok(boxed(parse_search(&output.stdout)))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("refresh"));
        }
        // swupd has no dedicated index verb; `search-file --init` is its
        // documented "download all required manifests, then exit".
        let mut cmd = self.cmd().args(["search-file", "--init"]).elevated(true);
        if ctx.assume_yes {
            cmd = cmd.arg("--assume=yes");
        }
        self.run(cmd, ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if !packages.is_empty() {
            return Err(Error::Other(format!(
                "{ID}: the OS updates as one unit; per-bundle upgrade is not supported"
            )));
        }
        if ctx.dry_run {
            return Err(self.no_dry_run("update"));
        }
        self.run(self.mutation("update", ctx), ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("check-update")
            .capture(&self.elevator, None)
            .await?;
        // Documented: 0 = update available, 1 = up to date, >1 = failure.
        match output.status.code() {
            Some(0) => {}
            Some(1) => return Ok(Vec::new()),
            _ => {
                output.require_success()?;
                return Ok(Vec::new());
            }
        }
        let (current, latest) = parse_check_update(&output.stdout);
        // The whole OS is the upgrade unit; one entry stands in for it.
        Ok(vec![Box::new(SwupdPackage {
            name: "clear-linux-os".to_string(),
            version: current,
            latest_version: latest,
            state: InstallState::Upgradable,
            ..Default::default()
        })])
    }
}

fn boxed(packages: Vec<SwupdPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `bundle-list`: ` - name` lines between a header and the `Total:`
/// summary.
fn parse_bundle_list(stdout: &str) -> Vec<SwupdPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let name = line.trim().strip_prefix("- ")?.split_whitespace().next()?;
            Some(SwupdPackage {
                name: name.to_string(),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// swupd-search results: `name - description` lines (descriptions wrap onto
/// following lines) separated by blank-line-delimited prose headers, which
/// never match the `name - ` shape.
fn parse_search(stdout: &str) -> Vec<SwupdPackage> {
    let mut packages: Vec<SwupdPackage> = Vec::new();
    let mut in_entry = false;
    for line in stdout.lines() {
        let text = line.trim();
        if text.is_empty() {
            in_entry = false;
            continue;
        }
        if let Some((name, description)) = text.split_once(" - ")
            && !name.contains(char::is_whitespace)
            && !name.is_empty()
        {
            packages.push(SwupdPackage {
                name: name.to_string(),
                description: Some(description.trim().to_string()),
                state: if text.contains("(installed)") {
                    InstallState::Installed
                } else {
                    InstallState::Available
                },
                ..Default::default()
            });
            in_entry = true;
            continue;
        }
        // A wrapped description continues its entry only with no blank line
        // in between; suggestion lines like `swupd bundle-add x` never do.
        if in_entry
            && !text.starts_with("swupd ")
            && let Some(last) = packages.last_mut()
            && let Some(description) = &mut last.description
        {
            description.push(' ');
            description.push_str(text);
        }
    }
    packages
}

/// `bundle-info`: prose around an ` Info for bundle: name` header, a
/// `Status:` line, and OS-version fields (`Installed bundle last updated in
/// version`, `Latest available version`) - bundle versions ARE OS versions.
fn parse_bundle_info(stdout: &str) -> Option<SwupdPackage> {
    let mut package = SwupdPackage::default();
    let mut update_available = false;
    for line in stdout.lines() {
        let text = line.trim();
        if let Some(name) = text.strip_prefix("Info for bundle:") {
            package.name = name.trim().to_string();
        } else if let Some(status) = text.strip_prefix("Status:") {
            package.state = if status.trim().starts_with("Not installed") {
                InstallState::Available
            } else {
                InstallState::Installed
            };
        } else if text.starts_with("There is an update for bundle") {
            update_available = true;
        } else if let Some(version) = text.strip_prefix("- Installed bundle last updated in version:")
        {
            package.version = Some(version.trim().to_string());
        } else if let Some(version) = text
            .strip_prefix("- Latest available version:")
            .or_else(|| text.strip_prefix("Latest available version:"))
        {
            package.latest_version = Some(version.trim().to_string());
        }
    }
    if package.name.is_empty() {
        return None;
    }
    if update_available && package.state == InstallState::Installed {
        package.state = InstallState::Upgradable;
    }
    // Not-installed bundles only report the server side.
    if package.version.is_none() {
        package.version = package.latest_version.take();
    } else if package.latest_version == package.version {
        package.latest_version = None;
    }
    Some(package)
}

/// `check-update`: `Current OS version: N` and `Latest server version: N`
/// lines; the verdict itself travels in the exit code.
fn parse_check_update(stdout: &str) -> (Option<String>, Option<String>) {
    let mut current = None;
    let mut latest = None;
    for line in stdout.lines() {
        let text = line.trim();
        if let Some(version) = text.strip_prefix("Current OS version:") {
            current = Some(version.trim().to_string());
        } else if let Some(version) = text.strip_prefix("Latest server version:") {
            latest = Some(version.trim().to_string());
        }
    }
    (current, latest)
}

/// A bundle as swupd describes it.
#[derive(Debug, Default)]
pub struct SwupdPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for SwupdPackage {
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

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bundle_list() {
        let stdout = "\
Installed bundles:
 - editors
 - os-core
 - os-core-update

Total: 3
";
        let packages = parse_bundle_list(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "editors");
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[2].name, "os-core-update");
    }

    #[test]
    fn parses_search_results_with_wrapped_descriptions() {
        let stdout = "\
Bundle with the best search result:

containers-virt - Run container applications from Dockerhub in
lightweight virtual machines

This bundle can be installed with:

     swupd bundle-add  containers-virt

Alternative bundle options are

     cloud-native-basic - Contains ClearLinux native software for Cloud
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "containers-virt");
        assert_eq!(
            packages[0].description.as_deref(),
            Some("Run container applications from Dockerhub in lightweight virtual machines")
        );
        assert_eq!(packages[1].name, "cloud-native-basic");
        assert_eq!(packages[1].state, InstallState::Available);
    }

    #[test]
    fn parses_bundle_info_with_pending_update() {
        let stdout = "\
 Info for bundle: editors
 Status: Explicitly installed

 There is an update for bundle editors:
  - Installed bundle last updated in version: 35480
  - Latest available version: 35490

 Bundle size:
  - Size of bundle: 12.9 MB
";
        let package = parse_bundle_info(stdout).unwrap();
        assert_eq!(package.name, "editors");
        assert_eq!(package.version.as_deref(), Some("35480"));
        assert_eq!(package.latest_version.as_deref(), Some("35490"));
        assert_eq!(package.state, InstallState::Upgradable);
    }

    #[test]
    fn parses_bundle_info_for_uninstalled_bundle() {
        let stdout = "\
 Info for bundle: c-basic
 Status: Not installed

 Latest available version: 35490

 Bundle size:
  - Maximum amount of disk size the bundle will take if installed (dependencies): 1.5 GB
";
        let package = parse_bundle_info(stdout).unwrap();
        assert_eq!(package.name, "c-basic");
        assert_eq!(package.version.as_deref(), Some("35490"));
        assert_eq!(package.latest_version, None);
        assert_eq!(package.state, InstallState::Available);
    }

    #[test]
    fn parses_check_update_versions() {
        let stdout = "\
Current OS version: 35480
Latest server version: 35490
There is a new OS version available: 35490
";
        let (current, latest) = parse_check_update(stdout);
        assert_eq!(current.as_deref(), Some("35480"));
        assert_eq!(latest.as_deref(), Some("35490"));
    }

    #[test]
    fn parses_check_update_when_current() {
        let stdout = "\
Current OS version: 35490
Latest server version: 35490
There are no updates available
";
        let (current, latest) = parse_check_update(stdout);
        assert_eq!(current.as_deref(), Some("35490"));
        assert_eq!(latest.as_deref(), Some("35490"));
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("editors@35480")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("editors")]).is_ok());
    }
}
