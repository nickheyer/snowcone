//! slapt-get backend for snowcone.
//!
//! An apt-get workalike for Slackware: long-flag verbs (`--install`,
//! `--remove`, `--update`, `--upgrade`, `--search`, `--show`,
//! `--installed`), a native `--simulate` dry run on every transaction, and
//! `--no-prompt` for yes. Messages are gettext-translated, so parsed reads
//! run under `LC_ALL=C`. slapt-get's version strings bundle
//! `version-arch-build` into one field, which this backend splits back
//! apart. There is no targeted upgrade verb - `--install` on an installed
//! package upgrades it, slapt-get's own documented behavior. The outdated
//! listing parses the apt-style "will be upgraded" section of
//! `--upgrade --simulate`, which names packages without versions, so
//! versions stay unset there.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "slapt-get";
const PROGRAMS: &[&str] = &["slapt-get"];

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

    /// Shared flags for mutating commands: elevated, `--no-prompt`, and the
    /// native simulate switch.
    fn mutation(&self, ctx: &OpContext) -> Cmd {
        let mut cmd = Cmd::new(&self.program).elevated(true);
        if ctx.assume_yes {
            cmd = cmd.arg("--no-prompt");
        }
        if ctx.dry_run {
            cmd = cmd.arg("--simulate");
        }
        cmd
    }
}

/// slapt-get installs whatever version its package sources carry; there is
/// no pkg=ver syntax.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but slapt-get only installs what its sources carry"
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
        "slapt-get"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "slackware"
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
        let cmd = self
            .mutation(ctx)
            .arg("--install")
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = self
            .mutation(ctx)
            .arg("--remove")
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("--installed")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_listing(&output.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query()
            .arg("--show")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        parse_show(&output.stdout)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("--search")
            .arg(query)
            .capture(&self.elevator, None)
            .await?;
        let packages = parse_listing(&output.stdout);
        if packages.is_empty() {
            output.require_success()?;
        }
        Ok(boxed(packages))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: refresh has no dry-run mode")));
        }
        self.run(Cmd::new(&self.program).arg("--update").elevated(true), ctx)
            .await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        let cmd = if packages.is_empty() {
            self.mutation(ctx).arg("--upgrade")
        } else {
            // Reinstalling an installed package pulls the newest available
            // version - slapt-get's targeted upgrade.
            self.mutation(ctx)
                .arg("--install")
                .args(packages.iter().map(|package| package.name.as_str()))
        };
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["--upgrade", "--simulate"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_simulated_upgrade(&output.stdout)))
    }
}

fn boxed(packages: Vec<SlaptGetPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `--search`/`--installed` lines:
/// `name-version-arch-build [inst=yes|no]: description`.
fn parse_listing(stdout: &str) -> Vec<SlaptGetPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let (token, rest) = line.split_once(" [inst=")?;
            let (inst, description) = rest.split_once("]:")?;
            let (name, version, arch, _build) = split_entry(token.trim())?;
            Some(SlaptGetPackage {
                name,
                version: Some(version),
                architecture: Some(arch),
                description: Some(description.trim().to_string()).filter(|d| !d.is_empty()),
                state: if inst.trim() == "yes" {
                    InstallState::Installed
                } else {
                    InstallState::Available
                },
                ..Default::default()
            })
        })
        .collect()
}

/// `--show`: `Package Key: value` fields, the description on indented lines
/// under `Package Description:` (only the summary line is kept), install
/// state directly from `Package Installed: yes|no`. Only the first stanza
/// is read when several sources answer.
fn parse_show(stdout: &str) -> Option<SlaptGetPackage> {
    let mut package = SlaptGetPackage {
        state: InstallState::Available,
        ..Default::default()
    };
    let mut in_description = false;
    for line in stdout.lines() {
        if line.starts_with(char::is_whitespace) {
            if in_description
                && package.description.is_none()
                && let text = line.trim()
                && !text.is_empty()
            {
                package.description = Some(text.to_string());
            }
            continue;
        }
        in_description = false;
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Package Name" => {
                if !package.name.is_empty() {
                    break;
                }
                package.name = value.to_string();
            }
            "Package Version" => {
                let (version, arch) = split_version(value);
                package.version = Some(version);
                package.architecture = arch;
            }
            "Package Mirror" if !value.is_empty() => package.origin = Some(value.to_string()),
            "Package Size" => package.download_size = parse_size(value),
            "Package Installed Size" => package.installed_size = parse_size(value),
            "Package Required" if !value.is_empty() => {
                package.dependencies = Some(
                    value
                        .split(',')
                        .filter_map(|dep| dep.split_whitespace().next())
                        .map(str::to_string)
                        .collect(),
                );
            }
            "Package Description" => in_description = true,
            "Package Installed" => {
                package.state = if value == "yes" {
                    InstallState::Installed
                } else {
                    InstallState::Available
                };
            }
            _ => {}
        }
    }
    (!package.name.is_empty()).then_some(package)
}

/// `--upgrade --simulate`: package names sit indented under the apt-style
/// "The following packages will be upgraded:" header; the unindented
/// summary line ends the section.
fn parse_simulated_upgrade(stdout: &str) -> Vec<SlaptGetPackage> {
    let mut packages = Vec::new();
    let mut in_section = false;
    for line in stdout.lines() {
        if line.starts_with("The following packages will be upgraded") {
            in_section = true;
            continue;
        }
        if !in_section {
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            break;
        }
        for name in line.split_whitespace() {
            packages.push(SlaptGetPackage {
                name: name.to_string(),
                state: InstallState::Upgradable,
                ..Default::default()
            });
        }
    }
    packages
}

/// Split the full `name-version-arch-build` token from the right - package
/// names may contain hyphens, the last three fields may not.
fn split_entry(entry: &str) -> Option<(String, String, String, String)> {
    let mut fields = entry.rsplitn(4, '-');
    let build = fields.next()?;
    let arch = fields.next()?;
    let version = fields.next()?;
    let name = fields.next()?;
    if name.is_empty() || version.is_empty() || arch.is_empty() || build.is_empty() {
        return None;
    }
    Some((
        name.to_string(),
        version.to_string(),
        arch.to_string(),
        build.to_string(),
    ))
}

/// slapt-get's version field is `version-arch-build`; split the version
/// proper from the architecture, keeping the whole string when the field
/// does not have all three parts.
fn split_version(value: &str) -> (String, Option<String>) {
    let mut fields = value.rsplitn(3, '-');
    let (Some(_build), Some(arch), Some(version)) = (fields.next(), fields.next(), fields.next())
    else {
        return (value.to_string(), None);
    };
    if version.is_empty() || arch.is_empty() {
        return (value.to_string(), None);
    }
    (version.to_string(), Some(arch.to_string()))
}

/// Sizes as slapt-get prints them: `290 K`, `1.2 M`.
fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let unit_at = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let number: f64 = text[..unit_at].parse().ok()?;
    let factor = match text[unit_at..].trim() {
        "" | "B" => 1.0,
        "K" | "KB" => 1024.0,
        "M" | "MB" => 1024.0 * 1024.0,
        "G" | "GB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((number * factor) as u64)
}

/// A package as slapt-get describes it.
#[derive(Debug, Default)]
pub struct SlaptGetPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub architecture: Option<String>,
    pub origin: Option<String>,
    pub installed_size: Option<u64>,
    pub download_size: Option<u64>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for SlaptGetPackage {
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

    fn architecture(&self) -> Option<&str> {
        self.architecture.as_deref()
    }

    fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
    }

    fn installed_size(&self) -> Option<u64> {
        self.installed_size
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
    fn parses_listing_lines() {
        let stdout = "\
gslapt-0.5.4a-x86_64-1 [inst=no]: gslapt (GTK slapt-get, an APT like system for Slackware)
mozilla-nss-3.101-x86_64-1 [inst=yes]: mozilla-nss (Network Security Services)
";
        let packages = parse_listing(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "gslapt");
        assert_eq!(packages[0].version.as_deref(), Some("0.5.4a"));
        assert_eq!(packages[0].architecture.as_deref(), Some("x86_64"));
        assert_eq!(packages[0].state, InstallState::Available);
        assert_eq!(
            packages[0].description.as_deref(),
            Some("gslapt (GTK slapt-get, an APT like system for Slackware)")
        );
        assert_eq!(packages[1].name, "mozilla-nss");
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn parses_show_output() {
        let stdout = "\
Package Name: gslapt
Package Mirror: http://software.jaos.org/slackpacks/15.0/
Package Priority: Default
Package Location: ./gslapt
Package Version: 0.5.4a-x86_64-1
Package Size: 290 K
Package Installed Size: 950 K
Package Required: slapt-get >= 0.9.11d,gtk+3 >= 3.24
Package Conflicts:
Package Suggests:
Package Description:
 gslapt (GTK slapt-get, an APT like system for Slackware)
Package Installed: yes
";
        let package = parse_show(stdout).unwrap();
        assert_eq!(package.name, "gslapt");
        assert_eq!(package.version.as_deref(), Some("0.5.4a"));
        assert_eq!(package.architecture.as_deref(), Some("x86_64"));
        assert_eq!(
            package.origin.as_deref(),
            Some("http://software.jaos.org/slackpacks/15.0/")
        );
        assert_eq!(package.download_size, Some(290 * 1024));
        assert_eq!(package.installed_size, Some(950 * 1024));
        assert_eq!(
            package.dependencies,
            Some(vec!["slapt-get".to_string(), "gtk+3".to_string()])
        );
        assert_eq!(
            package.description.as_deref(),
            Some("gslapt (GTK slapt-get, an APT like system for Slackware)")
        );
        assert_eq!(package.state, InstallState::Installed);
    }

    #[test]
    fn show_reads_only_the_first_stanza() {
        let stdout = "\
Package Name: xz
Package Version: 5.4.4-x86_64-1
Package Installed: no
Package Name: xz
Package Version: 5.2.5-x86_64-4
Package Installed: yes
";
        let package = parse_show(stdout).unwrap();
        assert_eq!(package.version.as_deref(), Some("5.4.4"));
        assert_eq!(package.state, InstallState::Available);
    }

    #[test]
    fn parses_simulated_upgrade_section() {
        let stdout = "\
Reading Package Lists... Done
The following packages will be upgraded:
  gslapt slapt-get
  mozilla-nss
2 upgraded, 0 reinstalled, 1 newly installed, 0 to remove and 0 not upgraded.
Done
";
        let packages = parse_simulated_upgrade(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "gslapt");
        assert_eq!(packages[2].name, "mozilla-nss");
        assert!(packages.iter().all(|p| p.state == InstallState::Upgradable));
    }

    #[test]
    fn simulated_upgrade_without_section_is_empty() {
        let stdout = "Reading Package Lists... Done\n0 upgraded.\n";
        assert!(parse_simulated_upgrade(stdout).is_empty());
    }

    #[test]
    fn splits_entries_and_versions() {
        let (name, version, arch, build) = split_entry("gcc-g++-13.2.0-x86_64-1").unwrap();
        assert_eq!(name, "gcc-g++");
        assert_eq!(version, "13.2.0");
        assert_eq!(arch, "x86_64");
        assert_eq!(build, "1");

        assert_eq!(
            split_version("0.5.4a-x86_64-1"),
            ("0.5.4a".to_string(), Some("x86_64".to_string()))
        );
        assert_eq!(split_version("0.5.4a"), ("0.5.4a".to_string(), None));
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("gslapt@0.5.4a")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("gslapt")]).is_ok());
    }
}
