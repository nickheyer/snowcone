//! Zypper backend for snowcone.
//!
//! zypper's global `--non-interactive` is the yes-flag, and install/remove/
//! update all take a native `--dry-run`. Exit codes 102 and 103 are
//! informational successes (reboot needed / zypper updated itself mid-run),
//! and `search` exits 104 when nothing matches. Table reads (`search`,
//! `list-updates`) parse the `|`-separated layout under `LC_ALL=C`;
//! `zypper info` is `Key : Value`, with the installed version of an
//! out-of-date package buried in its `Status` line.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "zypper";
const PROGRAMS: &[&str] = &["zypper"];

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
    /// streamed otherwise. 102 (reboot needed) and 103 (zypper restarted
    /// itself) report success, not failure.
    async fn run(&self, cmd: Cmd, ctx: &OpContext) -> Result<()> {
        let output = match &ctx.events {
            Some(events) => cmd.capture(&self.elevator, Some(events)).await?,
            None => cmd.run_interactive(&self.elevator).await?,
        };
        if matches!(output.status.code(), Some(102 | 103)) {
            return Ok(());
        }
        output.require_success()?;
        Ok(())
    }

    /// Shared shape for mutating commands: elevated, the global
    /// `--non-interactive` on `assume_yes`, `--dry-run` on `dry_run`.
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().elevated(true);
        if ctx.assume_yes {
            cmd = cmd.arg("--non-interactive");
        }
        cmd = cmd.arg(subcommand);
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        cmd
    }
}

/// `name=version` when the request pins one, bare name otherwise.
fn spec(request: &PackageRequest) -> String {
    match &request.version {
        Some(version) => format!("{}={version}", request.name),
        None => request.name.clone(),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Zypper"
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
            .mutation("install", ctx)
            .args(packages.iter().map(spec));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = self
            .mutation("remove", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["search", "--installed-only", "--details"])
            .capture(&self.elevator, None)
            .await?;
        // 104: nothing matched, i.e. nothing installed.
        if output.status.code() == Some(104) {
            return Ok(Vec::new());
        }
        let output = output.require_success()?;
        Ok(boxed(parse_installed(&output.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query()
            .args(["info", name])
            .capture(&self.elevator, None)
            .await?;
        // Older zypper exits 0 for an unknown package and just prints
        // "package 'x' not found" - the missing Name field catches that.
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        parse_info(&output.stdout)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["search", query])
            .capture(&self.elevator, None)
            .await?;
        // 104: no matches.
        if output.status.code() == Some(104) {
            return Ok(Vec::new());
        }
        let output = output.require_success()?;
        Ok(boxed(parse_search(&output.stdout)))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: refresh has no dry-run mode")));
        }
        self.run(self.mutation("refresh", ctx), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if packages.is_empty() {
            return self.run(self.mutation("update", ctx), ctx).await;
        }
        // `update` takes bare names only; a pinned request routes through
        // `install`, which moves an installed package to the named version.
        let pinned = packages.iter().any(|package| package.version.is_some());
        let cmd = if pinned {
            self.mutation("install", ctx)
                .args(packages.iter().map(spec))
        } else {
            self.mutation("update", ctx)
                .args(packages.iter().map(|package| package.name.as_str()))
        };
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("list-updates")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_updates(&output.stdout)))
    }
}

fn boxed(packages: Vec<ZypperPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// Split one `|`-separated table row into trimmed cells, rejecting the
/// `---+---` rules and anything that is not a table row at all.
fn cells(line: &str) -> Option<Vec<&str>> {
    if !line.contains('|') || line.contains("--+") {
        return None;
    }
    Some(line.split('|').map(str::trim).collect())
}

/// `zypper search`: `S | Name | Summary | Type` rows; `i`/`i+` in the status
/// column means installed, `v` means another version is installed.
fn parse_search(stdout: &str) -> Vec<ZypperPackage> {
    stdout
        .lines()
        .filter_map(cells)
        .filter_map(|cells| {
            let [status, name, summary, kind] = cells.as_slice() else {
                return None;
            };
            if *name == "Name" || *kind != "package" {
                return None;
            }
            Some(ZypperPackage {
                name: name.to_string(),
                description: Some(summary.to_string()).filter(|s| !s.is_empty()),
                state: if status.starts_with('i') || status.starts_with('v') {
                    InstallState::Installed
                } else {
                    InstallState::Available
                },
                ..Default::default()
            })
        })
        .collect()
}

/// `zypper search --installed-only --details`:
/// `S | Name | Type | Version | Arch | Repository` rows.
fn parse_installed(stdout: &str) -> Vec<ZypperPackage> {
    stdout
        .lines()
        .filter_map(cells)
        .filter_map(|cells| {
            let [_, name, kind, version, arch, repository] = cells.as_slice() else {
                return None;
            };
            if *name == "Name" || *kind != "package" {
                return None;
            }
            Some(ZypperPackage {
                name: name.to_string(),
                version: Some(version.to_string()),
                architecture: Some(arch.to_string()),
                origin: Some(repository.to_string()).filter(|r| !r.is_empty()),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `zypper list-updates`:
/// `S | Repository | Name | Current Version | Available Version | Arch` rows.
fn parse_updates(stdout: &str) -> Vec<ZypperPackage> {
    stdout
        .lines()
        .filter_map(cells)
        .filter_map(|cells| {
            let [_, repository, name, current, available, arch] = cells.as_slice() else {
                return None;
            };
            if *name == "Name" {
                return None;
            }
            Some(ZypperPackage {
                name: name.to_string(),
                version: Some(current.to_string()),
                latest_version: Some(available.to_string()),
                architecture: Some(arch.to_string()),
                origin: Some(repository.to_string()),
                state: InstallState::Upgradable,
                ..Default::default()
            })
        })
        .collect()
}

/// `zypper info`: `Key : Value` lines (the indented description body is
/// skipped); `Installed`/`Status` carry the local state, and an out-of-date
/// status names the installed version as `(version X installed)`.
fn parse_info(stdout: &str) -> Option<ZypperPackage> {
    let mut package = ZypperPackage {
        state: InstallState::Available,
        ..Default::default()
    };
    let mut installed = false;
    let mut status = None;
    for line in stdout.lines() {
        if line.starts_with(char::is_whitespace) {
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
            "Name" => package.name = value.to_string(),
            "Version" => package.version = Some(value.to_string()),
            "Arch" => package.architecture = Some(value.to_string()),
            "Repository" => package.origin = Some(value.to_string()),
            "Summary" => package.description = Some(value.to_string()),
            "Upstream URL" => package.homepage = Some(value.to_string()),
            "Installed" => installed = value.starts_with("Yes"),
            "Status" => status = Some(value.to_string()),
            _ => {}
        }
    }
    if package.name.is_empty() {
        return None;
    }
    if installed {
        package.state = InstallState::Installed;
        if let Some(status) = status
            && status.starts_with("out-of-date")
        {
            package.state = InstallState::Upgradable;
            // "out-of-date (version 14.0.0-1.1 installed)"
            if let Some(current) = status
                .split_once("version ")
                .and_then(|(_, rest)| rest.split_once(" installed"))
                .map(|(version, _)| version.to_string())
            {
                package.latest_version = package.version.take();
                package.version = Some(current);
            }
        }
    }
    Some(package)
}

/// A package as zypper describes it.
#[derive(Debug, Default)]
pub struct ZypperPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub architecture: Option<String>,
    pub origin: Option<String>,
    pub state: InstallState,
}

impl Package for ZypperPackage {
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
    fn parses_search_table() {
        let stdout = "\
Loading repository data...
Reading installed packages...

S  | Name                    | Summary                               | Type
---+-------------------------+---------------------------------------+--------
i+ | ripgrep                 | Line-oriented search tool             | package
   | ripgrep-bash-completion | Bash completion for ripgrep           | package
v  | fd                      | Alternative to find                   | package
   | ripgrep                 | Line-oriented search tool             | srcpackage
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "ripgrep");
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(
            packages[1].description.as_deref(),
            Some("Bash completion for ripgrep")
        );
        assert_eq!(packages[1].state, InstallState::Available);
        assert_eq!(packages[2].state, InstallState::Installed);
    }

    #[test]
    fn parses_installed_details_table() {
        let stdout = "\
Loading repository data...
Reading installed packages...

S  | Name    | Type    | Version    | Arch   | Repository
---+---------+---------+------------+--------+-----------------------
i+ | bash    | package | 5.2.26-1.5 | x86_64 | Main Repository (OSS)
i  | ripgrep | package | 14.1.0-1.1 | x86_64 | (System Packages)
";
        let packages = parse_installed(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "bash");
        assert_eq!(packages[0].version.as_deref(), Some("5.2.26-1.5"));
        assert_eq!(packages[0].origin.as_deref(), Some("Main Repository (OSS)"));
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn parses_list_updates_table() {
        let stdout = "\
Loading repository data...
Reading installed packages...

S | Repository       | Name    | Current Version | Available Version | Arch
--+------------------+---------+-----------------+-------------------+-------
v | Main Update Repo | ripgrep | 14.0.0-1.1      | 14.1.0-1.1        | x86_64
";
        let packages = parse_updates(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "ripgrep");
        assert_eq!(packages[0].version.as_deref(), Some("14.0.0-1.1"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("14.1.0-1.1"));
        assert_eq!(packages[0].origin.as_deref(), Some("Main Update Repo"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn parses_info_of_outdated_install() {
        let stdout = "\
Loading repository data...
Reading installed packages...

Information for package ripgrep:
--------------------------------
Repository     : Main Update Repository
Name           : ripgrep
Version        : 14.1.0-1.1
Arch           : x86_64
Vendor         : openSUSE
Installed Size : 4.9 MiB
Installed      : Yes
Status         : out-of-date (version 14.0.0-1.1 installed)
Source package : ripgrep-14.1.0-1.1.src
Upstream URL   : https://github.com/BurntSushi/ripgrep
Summary        : Line-oriented search tool
Description    :
    ripgrep is a line-oriented search tool. Note: the binary is named rg.
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.name, "ripgrep");
        assert_eq!(package.version.as_deref(), Some("14.0.0-1.1"));
        assert_eq!(package.latest_version.as_deref(), Some("14.1.0-1.1"));
        assert_eq!(package.state, InstallState::Upgradable);
        assert_eq!(package.origin.as_deref(), Some("Main Update Repository"));
        assert_eq!(
            package.homepage.as_deref(),
            Some("https://github.com/BurntSushi/ripgrep")
        );
        assert_eq!(
            package.description.as_deref(),
            Some("Line-oriented search tool")
        );
    }

    #[test]
    fn parses_info_of_available_package() {
        let stdout = "\
Information for package fd:
---------------------------
Repository  : Main Repository
Name        : fd
Version     : 10.1.0-1.2
Arch        : x86_64
Installed   : No
Status      : not installed
Summary     : Alternative to find
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.state, InstallState::Available);
        assert_eq!(package.version.as_deref(), Some("10.1.0-1.2"));
    }

    #[test]
    fn missing_name_means_not_found() {
        assert!(parse_info("package 'nope' not found.\n").is_none());
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("ripgrep@14.1.0-1.1")),
            "ripgrep=14.1.0-1.1"
        );
        assert_eq!(spec(&PackageRequest::parse("ripgrep")), "ripgrep");
    }
}
