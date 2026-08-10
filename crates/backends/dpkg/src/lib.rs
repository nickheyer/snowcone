//! dpkg backend for snowcone.
//!
//! dpkg is the low-level Debian tool: no dependency resolution, no remote
//! operations. Its real install contract is a *path to a .deb file*
//! (`dpkg -i /path/to/pkg.deb`), so that is what this backend expects as
//! the request name. Reads go through `dpkg-query`: listing uses `-W` with
//! an explicit format string (far more stable than scraping `dpkg -l`
//! columns), and info uses `-s`, which only knows about installed packages.
//! `--dry-run` is native to install and remove; dpkg has no yes-flag (it
//! only prompts on conffile conflicts), so `assume_yes` passes nothing.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "dpkg";
const PROGRAMS: &[&str] = &["dpkg"];

/// `dpkg-query -W` format: tab-separated fields, one package per line.
/// dpkg-query itself expands the `\t`/`\n` escapes.
const LIST_FORMAT: &str =
    "${Package}\\t${Version}\\t${Architecture}\\t${Status}\\t${binary:Summary}\\n";

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
        // Reads use the query half of the dpkg suite; it ships in the same
        // package as dpkg itself.
        let query_program =
            find_program("dpkg-query").ok_or_else(|| Error::Unavailable(ID.to_string()))?;
        Ok(Box::new(Manager {
            program,
            query_program,
            elevator: Elevator::detect(host),
        }))
    }
}

struct Manager {
    program: PathBuf,
    query_program: PathBuf,
    elevator: Elevator,
}

impl Manager {
    /// Mutating invocation, in the user's locale (output is passed through).
    fn cmd(&self) -> Cmd {
        Cmd::new(&self.program)
    }

    /// `dpkg-query` invocation with a stable locale, so parsing survives
    /// i18n.
    fn query(&self) -> Cmd {
        Cmd::new(&self.query_program).env("LC_ALL", "C")
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

    /// Shared shape for the two mutations: elevated, with dpkg's native
    /// `--dry-run` when asked for.
    fn mutation(&self, flag: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().elevated(true);
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        cmd.arg(flag)
    }
}

/// dpkg installs whatever version the given .deb file carries; a
/// `name@version` request has nothing to resolve against.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but dpkg installs the version inside the given .deb file"
        ))),
        None => Ok(()),
    }
}

/// True when a dpkg `Status` value (`want ok status`) ends in `installed`.
fn status_installed(status: &str) -> bool {
    status.split_whitespace().next_back() == Some("installed")
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "dpkg"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "dpkg"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
    }

    fn needs_elevation(&self, operation: Operation) -> bool {
        matches!(
            operation,
            Operation::Install | Operation::Remove | Operation::Upgrade | Operation::Refresh
        )
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        let cmd = self
            .mutation("-i", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = self
            .mutation("-r", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["-W", "-f", LIST_FORMAT])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_list(&output.stdout)
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query()
            .arg("-s")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        // `-s` also answers for removed-but-not-purged packages; only a
        // status ending in `installed` counts as present.
        parse_status(&output.stdout)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }
}

/// `dpkg-query -W` with [`LIST_FORMAT`]: `name\tversion\tarch\tstatus\t`
/// `summary` lines. The database also carries removed-but-configured (`rc`)
/// entries, which are filtered out by status.
fn parse_list(stdout: &str) -> Vec<DpkgPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?;
            let version = fields.next()?;
            let architecture = fields.next();
            let status = fields.next()?;
            if !status_installed(status) {
                return None;
            }
            Some(DpkgPackage {
                name: name.to_string(),
                version: Some(version.to_string()),
                architecture: architecture.map(str::to_string),
                description: fields.next().filter(|s| !s.is_empty()).map(str::to_string),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `dpkg-query -s`: a `Key: Value` stanza; continuation lines are indented
/// and only the summary on the `Description:` line itself is kept.
/// Returns `None` unless the stanza describes an actually-installed package.
fn parse_status(stdout: &str) -> Option<DpkgPackage> {
    let mut package = DpkgPackage {
        state: InstallState::Installed,
        ..Default::default()
    };
    let mut installed = false;
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
            "Package" => package.name = value.to_string(),
            "Status" => installed = status_installed(value),
            "Version" => package.version = Some(value.to_string()),
            "Architecture" => package.architecture = Some(value.to_string()),
            "Section" => package.origin = Some(value.to_string()),
            "Homepage" => package.homepage = Some(value.to_string()),
            // Recorded in KiB.
            "Installed-Size" => {
                package.installed_size = value.parse::<u64>().ok().map(|kib| kib * 1024);
            }
            "Depends" => {
                package.dependencies = Some(
                    value
                        .split(',')
                        .filter_map(|dep| dep.split_whitespace().next())
                        .map(str::to_string)
                        .collect(),
                );
            }
            "Description" => package.description = Some(value.to_string()),
            _ => {}
        }
    }
    (installed && !package.name.is_empty()).then_some(package)
}

/// A package as the dpkg database describes it.
#[derive(Debug, Default)]
pub struct DpkgPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub architecture: Option<String>,
    pub origin: Option<String>,
    pub installed_size: Option<u64>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for DpkgPackage {
    fn manager(&self) -> &str {
        ID
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> Option<&str> {
        self.version.as_deref()
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

    fn installed_size(&self) -> Option<u64> {
        self.installed_size
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
    fn parses_query_list_lines() {
        let stdout = "\
bash\t5.2.21-2\tamd64\tinstall ok installed\tGNU Bourne Again SHell
ripgrep\t14.1.0-1\tamd64\tinstall ok installed\tline-oriented search tool
";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "ripgrep");
        assert_eq!(packages[1].version.as_deref(), Some("14.1.0-1"));
        assert_eq!(packages[1].architecture.as_deref(), Some("amd64"));
        assert_eq!(
            packages[1].description.as_deref(),
            Some("line-oriented search tool")
        );
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn filters_removed_but_configured_entries() {
        let stdout = "\
old-tool\t1.0-1\tamd64\tdeinstall ok config-files\tformerly installed
bash\t5.2.21-2\tamd64\tinstall ok installed\tGNU Bourne Again SHell
";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "bash");
    }

    #[test]
    fn parses_status_stanza() {
        let stdout = "\
Package: ripgrep
Status: install ok installed
Priority: optional
Section: utils
Installed-Size: 5150
Maintainer: Debian Rust Maintainers <pkg-rust-maintainers@alioth-lists.debian.net>
Architecture: amd64
Version: 14.1.0-1
Depends: libc6 (>= 2.34), libgcc-s1 (>= 4.2)
Homepage: https://github.com/BurntSushi/ripgrep
Description: line-oriented search tool
 ripgrep recursively searches the current directory.
";
        let package = parse_status(stdout).unwrap();
        assert_eq!(package.name, "ripgrep");
        assert_eq!(package.version.as_deref(), Some("14.1.0-1"));
        assert_eq!(package.origin.as_deref(), Some("utils"));
        assert_eq!(package.installed_size, Some(5150 * 1024));
        assert_eq!(
            package.dependencies,
            Some(vec!["libc6".to_string(), "libgcc-s1".to_string()])
        );
        assert_eq!(
            package.description.as_deref(),
            Some("line-oriented search tool")
        );
        assert_eq!(package.state, InstallState::Installed);
    }

    #[test]
    fn status_stanza_of_removed_package_is_none() {
        let stdout = "\
Package: old-tool
Status: deinstall ok config-files
Version: 1.0-1
";
        assert!(parse_status(stdout).is_none());
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("./ripgrep.deb@14.1.0")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("./ripgrep_14.1.0-1_amd64.deb")]).is_ok());
    }
}
