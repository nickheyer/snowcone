//! RPM backend for snowcone.
//!
//! rpm is the low-level end of the RPM family: it has no repositories, so
//! `install` takes paths to local `.rpm` files - `-U` rather than `-i`,
//! because `-U` installs when nothing is present and upgrades in place
//! otherwise, while `-i` happily stacks a second copy alongside an installed
//! package. rpm never prompts, so `assume_yes` has nothing to do; `--test`
//! gives install and remove a native dry run. Reads never trust the default
//! `-qa` output - an explicit tab-separated `--queryformat` under `LC_ALL=C`
//! is the only stable contract.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "rpm";
const PROGRAMS: &[&str] = &["rpm"];
/// One `name<TAB>version-release<TAB>arch<TAB>summary` record per line.
const QUERYFORMAT: &str = "%{NAME}\t%{VERSION}-%{RELEASE}\t%{ARCH}\t%{SUMMARY}\n";

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
}

/// rpm installs whatever the given `.rpm` files contain; a request that pins
/// a version has no repository to resolve it against.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but rpm installs from local .rpm files"
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
        "RPM"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "rpmdb"
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

    /// `packages` name paths to local `.rpm` files, not repository packages.
    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        let mut cmd = self.cmd().arg("-U").elevated(true);
        if ctx.dry_run {
            cmd = cmd.arg("--test");
        }
        cmd = cmd.args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let mut cmd = self.cmd().arg("-e").elevated(true);
        if ctx.dry_run {
            cmd = cmd.arg("--test");
        }
        cmd = cmd.args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["-qa", "--queryformat", QUERYFORMAT])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_query(&output.stdout)
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // `-qi` only answers for installed packages; anything else is
        // "package X is not installed" with a non-zero exit.
        let output = self
            .query()
            .args(["-qi", name])
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        let package = parse_info(&output.stdout).ok_or_else(|| Error::Parse {
            what: format!("{ID} -qi output"),
            detail: format!("no `Name` field for `{name}`"),
        })?;
        Ok(Box::new(package))
    }
}

/// `rpm -qa --queryformat`: tab-separated `name`, `version-release`, `arch`,
/// `summary` records; pseudo-packages (gpg-pubkey) report their arch as
/// `(none)`.
fn parse_query(stdout: &str) -> Vec<RpmPackage> {
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
            Some(RpmPackage {
                name: name.to_string(),
                version: Some(version.to_string()),
                architecture: Some(arch.to_string()).filter(|arch| arch != "(none)"),
                description: Some(summary.to_string()).filter(|summary| !summary.is_empty()),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `rpm -qi`: `Key : Value` lines; everything after the `Description` key is
/// free text that may itself contain colons, so field parsing stops there
/// (which also limits multi-stanza output to its first package).
fn parse_info(stdout: &str) -> Option<RpmPackage> {
    let mut package = RpmPackage {
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
        if key == "Description" {
            break;
        }
        if value.is_empty() || value == "(none)" {
            continue;
        }
        match key {
            "Name" => package.name = value.to_string(),
            "Version" => version = Some(value.to_string()),
            "Release" => release = Some(value.to_string()),
            "Architecture" => package.architecture = Some(value.to_string()),
            "License" => package.license = Some(value.to_string()),
            "Summary" => package.description = Some(value.to_string()),
            "URL" => package.homepage = Some(value.to_string()),
            "Size" => package.installed_size = value.parse().ok(),
            _ => {}
        }
    }
    package.version = match (version, release) {
        (Some(version), Some(release)) => Some(format!("{version}-{release}")),
        (version, None) => version,
        (None, _) => None,
    };
    (!package.name.is_empty()).then_some(package)
}

/// A package as rpm describes it.
#[derive(Debug, Default)]
pub struct RpmPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub architecture: Option<String>,
    pub installed_size: Option<u64>,
    pub state: InstallState,
}

impl Package for RpmPackage {
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
    fn parses_queryformat_records() {
        let stdout = "\
bash\t5.2.26-3.fc40\tx86_64\tThe GNU Bourne Again shell
ripgrep\t14.1.0-1.fc40\tx86_64\tLine-oriented search tool
gpg-pubkey\t18b8e74c-62f2920f\t(none)\tOpenPGP public key
";
        let packages = parse_query(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[1].name, "ripgrep");
        assert_eq!(packages[1].version.as_deref(), Some("14.1.0-1.fc40"));
        assert_eq!(packages[1].architecture.as_deref(), Some("x86_64"));
        assert_eq!(packages[1].state, InstallState::Installed);
        assert_eq!(packages[2].architecture, None);
    }

    #[test]
    fn parses_qi_stanza() {
        let stdout = "\
Name        : ripgrep
Version     : 14.1.0
Release     : 1.fc40
Architecture: x86_64
Install Date: Mon 07 Jul 2026 09:15:02 AM UTC
Group       : Unspecified
Size        : 8339847
License     : MIT OR Unlicense
Signature   : RSA/SHA256, Tue 02 Jul 2026 01:02:03 AM UTC, Key ID 0123456789abcdef
Source RPM  : ripgrep-14.1.0-1.fc40.src.rpm
Build Date  : Mon 01 Jul 2026 12:00:00 AM UTC
Build Host  : buildhw-x86-01.fedoraproject.org
Packager    : Fedora Project
Vendor      : Fedora Project
URL         : https://github.com/BurntSushi/ripgrep
Summary     : Line-oriented search tool
Description :
ripgrep is a line-oriented search tool that recursively searches
your current directory for a regex pattern. Note: the binary is named rg.
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.name, "ripgrep");
        assert_eq!(package.version.as_deref(), Some("14.1.0-1.fc40"));
        assert_eq!(package.architecture.as_deref(), Some("x86_64"));
        assert_eq!(package.license.as_deref(), Some("MIT OR Unlicense"));
        assert_eq!(
            package.description.as_deref(),
            Some("Line-oriented search tool")
        );
        assert_eq!(
            package.homepage.as_deref(),
            Some("https://github.com/BurntSushi/ripgrep")
        );
        assert_eq!(package.installed_size, Some(8339847));
        assert_eq!(package.state, InstallState::Installed);
    }

    #[test]
    fn info_without_name_is_rejected() {
        assert!(parse_info("package ripgrep is not installed\n").is_none());
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("ripgrep@14.1.0")]).is_err());
        assert!(
            reject_pins(&[PackageRequest::parse("./ripgrep-14.1.0-1.fc40.x86_64.rpm")]).is_ok()
        );
    }
}
