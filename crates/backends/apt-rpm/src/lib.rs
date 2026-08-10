//! APT-RPM backend for snowcone.
//!
//! Drives the apt port used on PCLinuxOS and ALT: the CLI is classic
//! `apt-get`/`apt-cache`, not modern `apt`, so there is no `apt list` -
//! installed-state queries go through `rpm` on the shared rpmdb instead.
//! `-s` gives a native dry run on install/remove/upgrade, and the outdated
//! listing parses the stable `Inst name [old] (new …)` simulate lines.
//! Targeted upgrades use `install` (the old apt base predates
//! `--only-upgrade`), which also installs the package when absent.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "apt-rpm";
const PROGRAMS: &[&str] = &["apt-get"];

pub fn factory() -> Box<dyn BackendFactory> {
    Box::new(Factory)
}

struct Factory;

impl BackendFactory for Factory {
    fn id(&self) -> &'static str {
        ID
    }

    fn detect(&self, host: &HostInfo) -> Detection {
        if !(host.os.is_like("altlinux") || host.os.is_like("pclinuxos")) {
            return Detection::Unavailable {
                reason: "not a altlinux / pclinuxos based distro".to_string(),
            };
        }
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
        // apt-cache ships alongside apt-get; rpm is a given on any rpmdb
        // host. Both are load-bearing for reads.
        let cache_program =
            find_program("apt-cache").ok_or_else(|| Error::Unavailable(ID.to_string()))?;
        let rpm_program = find_program("rpm").ok_or_else(|| Error::Unavailable(ID.to_string()))?;
        Ok(Box::new(Manager {
            program,
            cache_program,
            rpm_program,
            elevator: Elevator::detect(host),
        }))
    }
}

struct Manager {
    program: PathBuf,
    cache_program: PathBuf,
    rpm_program: PathBuf,
    elevator: Elevator,
}

impl Manager {
    /// Mutating `apt-get` invocation, in the user's locale (output is
    /// passed through).
    fn cmd(&self) -> Cmd {
        Cmd::new(&self.program)
    }

    /// Read invocation with a stable locale, so parsing survives i18n.
    fn query(&self, program: &PathBuf) -> Cmd {
        Cmd::new(program).env("LC_ALL", "C")
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

    /// Shared flags for mutating commands: `-y` and the simulate switch.
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(subcommand).elevated(true);
        if ctx.assume_yes {
            cmd = cmd.arg("-y");
        }
        if ctx.dry_run {
            cmd = cmd.arg("-s");
        }
        cmd
    }

    /// Installed version of `name` from the rpmdb, `None` when absent.
    async fn installed_version(&self, name: &str) -> Result<Option<String>> {
        let output = self
            .query(&self.rpm_program)
            .args(["-q", "--queryformat", "%{VERSION}-%{RELEASE}\\n"])
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Ok(None);
        }
        Ok(output
            .stdout
            .lines()
            .next()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty()))
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
        "APT-RPM"
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
            .query(&self.rpm_program)
            .args([
                "-qa",
                "--queryformat",
                "%{NAME}\\t%{VERSION}-%{RELEASE}\\t%{ARCH}\\n",
            ])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_rpm_list(&output.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let show = self
            .query(&self.cache_program)
            .arg("show")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !show.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        let mut package =
            parse_show(&show.stdout).ok_or_else(|| Error::NotFound(name.to_string()))?;
        // `apt-cache show` only describes the repository side; the rpmdb
        // says whether (and at which version) it is installed.
        if let Some(installed) = self.installed_version(&package.name).await? {
            package.state = InstallState::Installed;
            if package.version.as_deref() != Some(installed.as_str()) {
                package.state = InstallState::Upgradable;
                package.latest_version = package.version.take();
            }
            package.version = Some(installed);
        }
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query(&self.cache_program)
            .arg("search")
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_search(&output.stdout)))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: refresh has no dry-run mode")));
        }
        self.run(self.cmd().arg("update").elevated(true), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = if packages.is_empty() {
            self.mutation("dist-upgrade", ctx)
        } else {
            self.mutation("install", ctx)
                .args(packages.iter().map(spec))
        };
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query(&self.program)
            .args(["-s", "dist-upgrade"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_outdated(&output.stdout)))
    }
}

fn boxed(packages: Vec<AptRpmPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `rpm -qa` with an explicit queryformat: `name\tversion-release\tarch`
/// lines.
fn parse_rpm_list(stdout: &str) -> Vec<AptRpmPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next().filter(|name| !name.is_empty())?;
            Some(AptRpmPackage {
                name: name.to_string(),
                version: fields.next().map(str::to_string),
                architecture: fields.next().map(str::to_string),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `apt-cache search`: `name - description` lines.
fn parse_search(stdout: &str) -> Vec<AptRpmPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let (name, description) = line.split_once(" - ")?;
            let name = name.trim();
            if name.is_empty() || name.contains(char::is_whitespace) {
                return None;
            }
            Some(AptRpmPackage {
                name: name.to_string(),
                description: Some(description.trim().to_string()),
                state: InstallState::Available,
                ..Default::default()
            })
        })
        .collect()
}

/// `apt-cache show`: `Key: Value` stanzas, one per known version; only the
/// first stanza is read. The indented long description is skipped -
/// only the summary on the `Description:` line is kept.
fn parse_show(stdout: &str) -> Option<AptRpmPackage> {
    let mut package = AptRpmPackage {
        state: InstallState::Available,
        ..Default::default()
    };
    let mut started = false;
    for line in stdout.lines() {
        if line.trim().is_empty() {
            if started {
                break;
            }
            continue;
        }
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
            "Package" => {
                package.name = value.to_string();
                started = true;
            }
            "Version" => package.version = Some(value.to_string()),
            "Architecture" => package.architecture = Some(value.to_string()),
            "Section" => package.origin = Some(value.to_string()),
            "Homepage" => package.homepage = Some(value.to_string()),
            // Bytes, when apt prints it as a bare number.
            "Size" => package.download_size = value.parse().ok(),
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
    (!package.name.is_empty()).then_some(package)
}

/// `apt-get -s dist-upgrade`: classic simulate output; upgrades appear as
/// `Inst name [old] (new origin …)` lines. `Inst` lines without a
/// bracketed old version are new installs, not upgrades, and are skipped.
fn parse_outdated(stdout: &str) -> Vec<AptRpmPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("Inst ")?;
            let mut parts = rest.split_whitespace();
            let name = parts.next()?;
            let old = parts
                .next()
                .and_then(|old| old.strip_prefix('['))
                .map(|old| old.trim_end_matches(']'))?;
            let new = parts
                .next()
                .and_then(|new| new.strip_prefix('('))
                .map(|new| new.trim_end_matches(')'))?;
            Some(AptRpmPackage {
                name: name.to_string(),
                version: Some(old.to_string()),
                latest_version: Some(new.to_string()),
                state: InstallState::Upgradable,
                ..Default::default()
            })
        })
        .collect()
}

/// A package as apt-rpm describes it.
#[derive(Debug, Default)]
pub struct AptRpmPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub architecture: Option<String>,
    pub origin: Option<String>,
    pub download_size: Option<u64>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for AptRpmPackage {
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

    fn download_size(&self) -> Option<u64> {
        self.download_size
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
    fn parses_rpm_list_lines() {
        let stdout = "\
bash\t5.2.15-alt1\tx86_64
zlib\t1.3.1-alt1\tx86_64
";
        let packages = parse_rpm_list(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "bash");
        assert_eq!(packages[0].version.as_deref(), Some("5.2.15-alt1"));
        assert_eq!(packages[0].architecture.as_deref(), Some("x86_64"));
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn parses_search_lines() {
        let stdout = "\
ripgrep - Line-oriented search tool
ripgrep-debuginfo - Debug information for package ripgrep
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "ripgrep");
        assert_eq!(
            packages[0].description.as_deref(),
            Some("Line-oriented search tool")
        );
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn parses_first_show_stanza_only() {
        let stdout = "\
Package: ripgrep
Section: Text tools
Installed Size: 5271234
Maintainer: Someone <someone@altlinux.org>
Version: 14.1.0-alt1
Architecture: x86_64
Size: 1620322
Depends: glibc-core (>= 2.32), libpcre2
Description: Line-oriented search tool
 ripgrep recursively searches the current directory.

Package: ripgrep
Version: 13.0.0-alt1
Description: Older stanza that must be ignored
";
        let package = parse_show(stdout).unwrap();
        assert_eq!(package.name, "ripgrep");
        assert_eq!(package.version.as_deref(), Some("14.1.0-alt1"));
        assert_eq!(package.origin.as_deref(), Some("Text tools"));
        assert_eq!(package.download_size, Some(1620322));
        assert_eq!(
            package.dependencies,
            Some(vec!["glibc-core".to_string(), "libpcre2".to_string()])
        );
        assert_eq!(
            package.description.as_deref(),
            Some("Line-oriented search tool")
        );
    }

    #[test]
    fn parses_simulate_upgrade_lines() {
        let stdout = "\
Reading Package Lists...
Building Dependency Tree...
The following packages will be upgraded
  bash zlib
Inst bash [5.2.15-alt1] (5.2.21-alt1 Sisyphus:x86_64)
Inst new-dep (1.0-alt1 Sisyphus:x86_64)
Conf bash (5.2.21-alt1 Sisyphus:x86_64)
";
        let packages = parse_outdated(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "bash");
        assert_eq!(packages[0].version.as_deref(), Some("5.2.15-alt1"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("5.2.21-alt1"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("ripgrep@14.1.0-alt1")),
            "ripgrep=14.1.0-alt1"
        );
        assert_eq!(spec(&PackageRequest::parse("ripgrep")), "ripgrep");
    }
}
