//! transactional-update backend for snowcone.
//!
//! openSUSE MicroOS's updater wraps zypper inside a new btrfs snapshot:
//! every mutation lands in that snapshot and only takes effect after a
//! reboot. The tool must run as root, so snowcone elevates all mutations;
//! `--non-interactive` (a general option, placed before the command) maps
//! `assume_yes` onto the wrapped zypper. The running system's rpmdb stays
//! readable throughout, so the read side (list-installed, info) queries it
//! directly through the host `rpm` binary, and refresh goes straight to
//! `zypper refresh` - repo metadata lives on the writable /var subvolume,
//! outside the snapshot flow.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "transactional-update";
const PROGRAMS: &[&str] = &["transactional-update"];

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
            zypper: find_program("zypper"),
            elevator: Elevator::detect(host),
        }))
    }
}

struct Manager {
    program: PathBuf,
    /// Host `rpm`, for reading the running system's rpmdb.
    rpm: Option<PathBuf>,
    /// Host `zypper`, for refreshing repo metadata outside a snapshot.
    zypper: Option<PathBuf>,
    elevator: Elevator,
}

impl Manager {
    fn cmd(&self) -> Cmd {
        Cmd::new(&self.program)
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

    /// Shared shape for snapshot mutations: elevated, general options
    /// before the (possibly multi-word) command.
    fn mutation(&self, command: &[&str], ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().elevated(true);
        if ctx.assume_yes {
            cmd = cmd.arg("--non-interactive");
        }
        cmd.args(command.iter().copied())
    }

    fn no_dry_run(&self, operation: &str) -> Error {
        Error::Other(format!("{ID}: {operation} has no dry-run mode"))
    }
}

/// zypper-in-a-snapshot has no usable version selection here: installs
/// always take what the repos currently hold.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but snapshot installs always track the repository"
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
        "transactional-update"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "rpmdb"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::REFRESH | Capabilities::UPGRADE
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
            return Err(self.no_dry_run("pkg install"));
        }
        let cmd = self
            .mutation(&["pkg", "install"], ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("pkg remove"));
        }
        let cmd = self
            .mutation(&["pkg", "remove"], ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .rpm_query()?
            .args(["-qa", "--queryformat", "%{NAME}\t%{VERSION}-%{RELEASE}\t%{ARCH}\n"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_installed(&output.stdout)
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
            return Err(self.no_dry_run("refresh"));
        }
        // transactional-update has no refresh verb; metadata lives on the
        // writable /var, so plain `zypper refresh` is the honest path.
        let zypper = self.zypper.as_deref().ok_or_else(|| {
            Error::Other(format!("{ID}: `zypper` not found on PATH to refresh metadata"))
        })?;
        let mut cmd = Cmd::new(zypper).elevated(true);
        if ctx.assume_yes {
            cmd = cmd.arg("--non-interactive");
        }
        self.run(cmd.arg("refresh"), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        let cmd = if packages.is_empty() {
            self.mutation(&["up"], ctx)
        } else {
            self.mutation(&["pkg", "update"], ctx)
                .args(packages.iter().map(|package| package.name.as_str()))
        };
        self.run(cmd, ctx).await
    }
}

/// `rpm -qa` with a tab-separated `name\tversion-release\tarch` query
/// format, one package per line; pseudo-packages report their arch as
/// `(none)`.
fn parse_installed(stdout: &str) -> Vec<TransactionalUpdatePackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let name = parts.next()?.trim();
            if name.is_empty() {
                return None;
            }
            Some(TransactionalUpdatePackage {
                name: name.to_string(),
                version: parts.next().map(str::to_string),
                architecture: parts
                    .next()
                    .filter(|arch| *arch != "(none)")
                    .map(str::to_string),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `rpm -qi`: `Key : Value` fields with a multi-line description at the
/// end; `Version` and `Release` join into the full version string, and
/// `Size` is plain bytes.
fn parse_rpm_info(stdout: &str) -> Option<TransactionalUpdatePackage> {
    let mut package = TransactionalUpdatePackage {
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

/// A package as the running system's rpmdb describes it.
#[derive(Debug, Default)]
pub struct TransactionalUpdatePackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub architecture: Option<String>,
    pub installed_size: Option<u64>,
    pub state: InstallState,
}

impl Package for TransactionalUpdatePackage {
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
    fn parses_installed_query_lines() {
        let stdout = "\
bash\t5.2.15-9.1\tx86_64
kernel-default\t6.9.3-1.1\tx86_64
gpg-pubkey\t39db7c82-510a966b\t(none)
";
        let packages = parse_installed(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "bash");
        assert_eq!(packages[0].version.as_deref(), Some("5.2.15-9.1"));
        assert_eq!(packages[0].architecture.as_deref(), Some("x86_64"));
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[2].architecture, None);
    }

    #[test]
    fn parses_rpm_info_fields() {
        let stdout = "\
Name        : bash
Version     : 5.2.15
Release     : 9.1
Architecture: x86_64
Install Date: Wed May 15 08:14:02 2024
Size        : 1868216
License     : GPL-3.0-or-later
URL         : https://www.gnu.org/software/bash/
Summary     : The GNU Bourne-Again Shell
Description :
Bash is an sh-compatible command interpreter that executes commands
read from standard input.
";
        let package = parse_rpm_info(stdout).unwrap();
        assert_eq!(package.name, "bash");
        assert_eq!(package.version.as_deref(), Some("5.2.15-9.1"));
        assert_eq!(package.architecture.as_deref(), Some("x86_64"));
        assert_eq!(package.license.as_deref(), Some("GPL-3.0-or-later"));
        assert_eq!(
            package.homepage.as_deref(),
            Some("https://www.gnu.org/software/bash/")
        );
        assert_eq!(package.installed_size, Some(1868216));
        assert_eq!(
            package.description.as_deref(),
            Some("The GNU Bourne-Again Shell")
        );
        assert_eq!(package.state, InstallState::Installed);
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("bash@5.2.15")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("bash")]).is_ok());
    }
}
