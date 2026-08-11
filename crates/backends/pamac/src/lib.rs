//! Pamac backend for snowcone.
//!
//! Manjaro's CLI over the alpm database. pamac arbitrates privilege itself
//! through polkit, so snowcone never prefixes the elevation helper - but an
//! authentication prompt is still coming for mutations. install/remove/
//! update all take `--dry-run` natively. The AUR side of search and
//! checkupdates only appears when the user enabled AUR support in
//! pamac.conf; nothing here forces it. pamac has no verb that only
//! refreshes the sync databases (`checkupdates` syncs temporary copies),
//! so the REFRESH capability is dropped.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "pamac";
const PROGRAMS: &[&str] = &["pamac"];

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

    /// Shared flags for mutating subcommands: `--no-confirm` and the
    /// native `--dry-run`.
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(subcommand);
        if ctx.assume_yes {
            cmd = cmd.arg("--no-confirm");
        }
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        cmd
    }
}

/// alpm has no version selection: installs always take the repo/AUR head.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but alpm only installs the latest"
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
        "Pamac"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "alpm"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
    }

    /// pamac drives polkit itself for alpm mutations - snowcone never
    /// elevates it, but a credential prompt is still coming.
    fn needs_elevation(&self, operation: Operation) -> bool {
        operation.mutates()
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        let cmd = self
            .mutation("install", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
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
            .args(["list", "--installed"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_list(&output.stdout))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query()
            .arg("info")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        // pamac exits 1 with "Error: target not found" for unknown names.
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        match parse_info(&output.stdout) {
            Some(package) => Ok(Box::new(package)),
            None => Err(Error::NotFound(name.to_string())),
        }
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("search")
            .arg(query)
            .capture(&self.elevator, None)
            .await?;
        // pamac exits non-zero on "no matches" - an empty result, not an
        // error.
        if !output.success() && output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(parse_search(&output.stdout))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        let cmd = if packages.is_empty() {
            self.mutation("update", ctx)
        } else {
            // `pamac update` takes no targets; reinstalling through
            // `install` moves the named packages to the repo head.
            self.mutation("install", ctx)
                .args(packages.iter().map(|package| package.name.as_str()))
        };
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("checkupdates")
            .capture(&self.elevator, None)
            .await?;
        // checkupdates exits 100 when updates are available (documented in
        // its own --help), 0 when the system is up to date.
        if matches!(output.status.code(), Some(0) | Some(100)) {
            return Ok(parse_checkupdates(&output.stdout));
        }
        output.require_success()?;
        Ok(Vec::new())
    }
}

/// `list --installed`: `name version repo size` columns; the repo column is
/// blank for foreign packages, in which case the size (which starts with a
/// digit) slides into its place.
fn parse_list(stdout: &str) -> Vec<Box<dyn Package>> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            let version = parts.next()?;
            let origin = parts
                .next()
                .filter(|token| !token.starts_with(|c: char| c.is_ascii_digit()));
            Some(Box::new(PamacPackage {
                name: name.to_string(),
                version: Some(version.to_string()),
                origin: origin.map(str::to_string),
                state: InstallState::Installed,
                ..Default::default()
            }) as Box<dyn Package>)
        })
        .collect()
}

/// `search`: `name version [Installed] repo` headers (repo right-aligned,
/// possibly absent) with descriptions indented four spaces below each.
fn parse_search(stdout: &str) -> Vec<Box<dyn Package>> {
    let mut packages: Vec<PamacPackage> = Vec::new();
    for line in stdout.lines() {
        if line.starts_with(char::is_whitespace) {
            if let (Some(last), text) = (packages.last_mut(), line.trim())
                && !text.is_empty()
            {
                match &mut last.description {
                    Some(description) => {
                        description.push(' ');
                        description.push_str(text);
                    }
                    None => last.description = Some(text.to_string()),
                }
            }
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(name), Some(version)) = (parts.next(), parts.next()) else {
            continue;
        };
        let rest: Vec<&str> = parts.collect();
        let installed = rest.contains(&"[Installed]");
        let origin = rest.into_iter().rfind(|token| *token != "[Installed]");
        packages.push(PamacPackage {
            name: name.to_string(),
            version: Some(version.to_string()),
            origin: origin.map(str::to_string),
            state: if installed {
                InstallState::Installed
            } else {
                InstallState::Available
            },
            ..Default::default()
        });
    }
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `checkupdates`: `name current -> latest repo` rows under a count header;
/// lines without the `->` marker (headers, flatpak rows) are skipped.
fn parse_checkupdates(stdout: &str) -> Vec<Box<dyn Package>> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            let current = parts.next()?;
            if parts.next()? != "->" {
                return None;
            }
            let latest = parts.next()?;
            Some(Box::new(PamacPackage {
                name: name.to_string(),
                version: Some(current.to_string()),
                latest_version: Some(latest.to_string()),
                origin: parts.next().map(str::to_string),
                state: InstallState::Upgradable,
                ..Default::default()
            }) as Box<dyn Package>)
        })
        .collect()
}

/// `info`: `Key : Value` fields with the key padded to a fixed width and
/// wrapped values continued on indented lines; empty values print as `--`,
/// `None`, or `Unknown`, and the `Install Date` field only exists for
/// installed packages.
fn parse_info(stdout: &str) -> Option<PamacPackage> {
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in stdout.lines() {
        if line.starts_with(char::is_whitespace) || !line.contains(':') {
            if let Some((_, value)) = fields.last_mut() {
                let text = line.trim();
                if !text.is_empty() {
                    value.push(' ');
                    value.push_str(text);
                }
            }
            continue;
        }
        let (key, value) = line.split_once(':')?;
        fields.push((key.trim().to_string(), value.trim().to_string()));
    }
    let mut package = PamacPackage {
        state: InstallState::Available,
        ..Default::default()
    };
    for (key, value) in fields {
        if key == "Install Date" {
            package.state = InstallState::Installed;
        }
        if value.is_empty() || value == "None" || value == "--" || value == "Unknown" {
            continue;
        }
        match key.as_str() {
            "Name" => package.name = value,
            "Version" => package.version = Some(value),
            "Description" => package.description = Some(value),
            "URL" => package.homepage = Some(value),
            "Licenses" => package.license = Some(value),
            "Repository" => package.origin = Some(value),
            "Depends On" => {
                package.dependencies = Some(value.split_whitespace().map(str::to_string).collect());
            }
            _ => {}
        }
    }
    (!package.name.is_empty()).then_some(package)
}

/// A package as pamac describes it.
#[derive(Debug, Default)]
pub struct PamacPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub origin: Option<String>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for PamacPackage {
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

    fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
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
    fn parses_installed_list_columns() {
        let stdout = "\
acl                 2.3.2-1                  core       139.3 kB
firefox             128.0-1                  extra      231.2 MB
yay-bin             12.3.5-1                 6.5 MB
";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name(), "acl");
        assert_eq!(packages[0].version(), Some("2.3.2-1"));
        assert_eq!(packages[0].origin(), Some("core"));
        assert_eq!(packages[0].state(), InstallState::Installed);
        // Foreign package: blank repo column, size must not become one.
        assert_eq!(packages[2].name(), "yay-bin");
        assert_eq!(packages[2].origin(), None);
    }

    #[test]
    fn parses_search_headers_and_descriptions() {
        let stdout = "\
ripgrep-all  0.10.6-1                                          extra
    rga: ripgrep, but also search in PDFs, E-Books,
    Office documents, zip, tar.gz, etc.
firefox  128.0-1 [Installed]                                   extra
    Fast, Private & Safe Web Browser
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name(), "ripgrep-all");
        assert_eq!(packages[0].origin(), Some("extra"));
        assert_eq!(packages[0].state(), InstallState::Available);
        assert!(packages[0].description().unwrap().ends_with("tar.gz, etc."));
        assert_eq!(packages[1].name(), "firefox");
        assert_eq!(packages[1].state(), InstallState::Installed);
        assert_eq!(packages[1].origin(), Some("extra"));
    }

    #[test]
    fn parses_checkupdates_rows() {
        let stdout = "\
2 available updates:
firefox     127.0.2-1  -> 128.0-1    extra
lib32-mesa  24.1.1-1   -> 24.1.2-1   multilib
";
        let packages = parse_checkupdates(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name(), "firefox");
        assert_eq!(packages[0].version(), Some("127.0.2-1"));
        assert_eq!(packages[0].latest_version(), Some("128.0-1"));
        assert_eq!(packages[0].origin(), Some("extra"));
        assert_eq!(packages[0].state(), InstallState::Upgradable);
    }

    #[test]
    fn up_to_date_checkupdates_parses_to_nothing() {
        assert!(parse_checkupdates("Your system is up to date.\n").is_empty());
    }

    #[test]
    fn parses_installed_info_fields() {
        let stdout = "\
Name                  : firefox
Version               : 128.0-1
Description           : Fast, Private & Safe Web Browser
URL                   : https://www.mozilla.org/firefox/
Licenses              : MPL-2.0
Repository            : extra
Installed Size        : 231.2 MB
Groups                : --
Depends On            : gtk3 libxt mime-types dbus-glib nss
Optional Dependencies : networkmanager: Location detection via available
                        WiFi networks [Installed]
Install Date          : Sat 26 Mar 2026
Install Reason        : Explicitly installed
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.name, "firefox");
        assert_eq!(package.version.as_deref(), Some("128.0-1"));
        assert_eq!(package.origin.as_deref(), Some("extra"));
        assert_eq!(package.license.as_deref(), Some("MPL-2.0"));
        assert_eq!(
            package.homepage.as_deref(),
            Some("https://www.mozilla.org/firefox/")
        );
        assert_eq!(
            package.dependencies.as_deref().map(<[String]>::len),
            Some(5)
        );
        assert_eq!(package.state, InstallState::Installed);
    }

    #[test]
    fn info_without_install_date_is_available() {
        let stdout = "\
Name                  : ripgrep
Version               : 14.1.0-1
Description           : A search tool
Repository            : extra
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.state, InstallState::Available);
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("ripgrep@14.1.0")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("ripgrep")]).is_ok());
    }
}
