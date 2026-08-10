//! urpmi backend for snowcone.
//!
//! Mageia's urpmi is a suite, not one binary: `urpmi` installs and upgrades
//! (`--auto-select` for everything), `urpme` removes, `urpmq` queries, and
//! `urpmi.update -a` refreshes media - each companion is resolved separately
//! at startup. `--auto` answers prompts and `--test` is a native dry run for
//! install/upgrade. The suite has no list-installed verb at all; that read
//! honestly comes from `rpm -qa` with an explicit `--queryformat` against
//! the shared rpmdb.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "urpmi";
const PROGRAMS: &[&str] = &["urpmi"];
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
            urpme: find_program("urpme"),
            urpmq: find_program("urpmq"),
            urpmi_update: find_program("urpmi.update"),
            rpm: find_program("rpm"),
            elevator: Elevator::detect(host),
        }))
    }
}

struct Manager {
    program: PathBuf,
    urpme: Option<PathBuf>,
    urpmq: Option<PathBuf>,
    urpmi_update: Option<PathBuf>,
    rpm: Option<PathBuf>,
    elevator: Elevator,
}

impl Manager {
    /// Mutating `urpmi` invocation, in the user's locale (output is passed
    /// through), with the shared `--auto`/`--test` flag handling.
    fn urpmi(&self, ctx: &OpContext) -> Cmd {
        let mut cmd = Cmd::new(&self.program).elevated(true);
        if ctx.assume_yes {
            cmd = cmd.arg("--auto");
        }
        if ctx.dry_run {
            cmd = cmd.arg("--test");
        }
        cmd
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

/// Read invocation with a stable locale, so parsing survives i18n.
fn query(program: &Path) -> Cmd {
    Cmd::new(program).env("LC_ALL", "C")
}

/// The suite ships as separate binaries; a missing companion is an
/// incomplete installation, reported per operation.
fn companion<'a>(tool: &'a Option<PathBuf>, name: &str) -> Result<&'a PathBuf> {
    tool.as_ref().ok_or_else(|| {
        Error::Other(format!("{ID}: companion tool `{name}` not found on PATH"))
    })
}

/// urpmi resolves names against whatever its media carry; there is no
/// version-selection syntax.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but urpmi only installs what its media carry"
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
        "urpmi"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "rpmdb"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::SEARCH | Capabilities::REFRESH | Capabilities::UPGRADE
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
            .urpmi(ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: remove has no dry-run mode")));
        }
        let urpme = companion(&self.urpme, "urpme")?;
        let mut cmd = Cmd::new(urpme).elevated(true);
        if ctx.assume_yes {
            cmd = cmd.arg("--auto");
        }
        cmd = cmd.args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        // The suite has no list verb; the rpmdb both tools share does.
        let rpm = companion(&self.rpm, "rpm")?;
        let output = query(rpm)
            .args(["-qa", "--queryformat", QUERYFORMAT])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_rpm_query(&output.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let urpmq = companion(&self.urpmq, "urpmq")?;
        let output = query(urpmq)
            .args(["-i", name])
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        let Some(mut package) = parse_info(&output.stdout) else {
            return Err(Error::NotFound(name.to_string()));
        };
        // urpmq only sees the media side; the rpmdb answers "is it
        // installed" and with which version.
        if let Ok(rpm) = companion(&self.rpm, "rpm") {
            let probe = query(rpm)
                .args(["-q", "--queryformat", "%{VERSION}-%{RELEASE}\n", name])
                .capture(&self.elevator, None)
                .await?;
            if probe.success()
                && let Some(installed) = probe
                    .stdout
                    .lines()
                    .next()
                    .map(str::trim)
                    .filter(|version| !version.is_empty())
            {
                package.state = InstallState::Installed;
                if package.version.as_deref() != Some(installed) {
                    package.latest_version = package.version.take();
                    package.version = Some(installed.to_string());
                }
            }
        }
        Ok(Box::new(package))
    }

    async fn search(&self, query_text: &str) -> Result<Vec<Box<dyn Package>>> {
        let urpmq = companion(&self.urpmq, "urpmq")?;
        let output = query(urpmq)
            .args(["--fuzzy", query_text])
            .capture(&self.elevator, None)
            .await?;
        // No match exits non-zero with an empty stdout; that is an empty
        // result, not a failure.
        if !output.success() && output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(boxed(parse_names(&output.stdout)))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: refresh has no dry-run mode")));
        }
        let update = companion(&self.urpmi_update, "urpmi.update")?;
        self.run(Cmd::new(update).arg("-a").elevated(true), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        let cmd = if packages.is_empty() {
            self.urpmi(ctx).arg("--auto-select")
        } else {
            // Naming installed packages makes urpmi pull their newest media
            // version - there is no separate targeted-upgrade verb.
            self.urpmi(ctx)
                .args(packages.iter().map(|package| package.name.as_str()))
        };
        self.run(cmd, ctx).await
    }
}

fn boxed(packages: Vec<UrpmiPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `rpm -qa --queryformat`: tab-separated `name`, `version-release`, `arch`,
/// `summary` records; pseudo-packages (gpg-pubkey) report their arch as
/// `(none)`.
fn parse_rpm_query(stdout: &str) -> Vec<UrpmiPackage> {
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
            Some(UrpmiPackage {
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

/// `urpmq --fuzzy`: bare matching package names, one per line.
fn parse_names(stdout: &str) -> Vec<UrpmiPackage> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty() && !name.contains(char::is_whitespace))
        .map(|name| UrpmiPackage {
            name: name.to_string(),
            state: InstallState::Available,
            ..Default::default()
        })
        .collect()
}

/// `urpmq -i`: rpm-style `Key : Value` lines; everything after the
/// `Description` key is free text that may itself contain colons, so field
/// parsing stops there (limiting multi-media output to its first stanza).
fn parse_info(stdout: &str) -> Option<UrpmiPackage> {
    let mut package = UrpmiPackage {
        state: InstallState::Available,
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

/// A package as the urpmi suite describes it.
#[derive(Debug, Default)]
pub struct UrpmiPackage {
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

impl Package for UrpmiPackage {
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
    fn parses_rpm_queryformat_records() {
        let stdout = "\
bash\t5.2.26-3.mga9\tx86_64\tThe GNU Bourne Again shell
ripgrep\t14.1.0-1.mga9\tx86_64\tLine-oriented search tool
gpg-pubkey\t80420f66-63c7e73d\t(none)\tOpenPGP public key
";
        let packages = parse_rpm_query(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[1].name, "ripgrep");
        assert_eq!(packages[1].version.as_deref(), Some("14.1.0-1.mga9"));
        assert_eq!(packages[1].state, InstallState::Installed);
        assert_eq!(packages[2].architecture, None);
    }

    #[test]
    fn parses_fuzzy_name_list() {
        let stdout = "\
ripgrep
ripgrep-bash-completion

";
        let packages = parse_names(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "ripgrep");
        assert_eq!(packages[1].name, "ripgrep-bash-completion");
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn parses_urpmq_info_stanza() {
        let stdout = "\
Name        : ripgrep
Version     : 14.1.0
Release     : 1.mga9
Group       : Development/Other
Size        : 8339847
Architecture: x86_64
Source RPM  : ripgrep-14.1.0-1.mga9.src.rpm
URL         : https://github.com/BurntSushi/ripgrep
Summary     : Line-oriented search tool
Description :
ripgrep is a line-oriented search tool that recursively searches
your current directory for a regex pattern. Note: the binary is named rg.
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.name, "ripgrep");
        assert_eq!(package.version.as_deref(), Some("14.1.0-1.mga9"));
        assert_eq!(package.architecture.as_deref(), Some("x86_64"));
        assert_eq!(package.installed_size, Some(8339847));
        assert_eq!(
            package.description.as_deref(),
            Some("Line-oriented search tool")
        );
        assert_eq!(
            package.homepage.as_deref(),
            Some("https://github.com/BurntSushi/ripgrep")
        );
        assert_eq!(package.state, InstallState::Available);
    }

    #[test]
    fn info_without_name_is_rejected() {
        assert!(parse_info("no package named nope\n").is_none());
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("ripgrep@14.1.0")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("ripgrep")]).is_ok());
    }
}
