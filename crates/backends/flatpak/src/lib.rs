//! Flatpak backend for snowcone.
//!
//! Drives `flatpak` as a user-facing app manager: listings are scoped to
//! applications (`--app`) because runtimes are dependencies flatpak pulls
//! in and retires on its own. Package names are application IDs
//! (`org.gnome.Calculator`); installing a plain name makes flatpak search
//! every remote, which non-interactively only works when the match is
//! unambiguous. Reads always pass explicit `--columns=…`, which flatpak
//! prints tab-separated (and headerless) when piped. Flatpak arbitrates
//! system-vs-user access itself through polkit, so snowcone never
//! prefixes an elevation helper. No flatpak verb has a simulate flag, so
//! `--dry-run` always errors.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "flatpak";
const PROGRAMS: &[&str] = &["flatpak"];

const LIST_COLUMNS: &str = "--columns=application,version,name,origin";
const SEARCH_COLUMNS: &str = "--columns=application,version,remotes,description";
const UPDATE_COLUMNS: &str = "--columns=application,version,origin";

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

    /// Mutating command; `--noninteractive` suppresses flatpak's questions
    /// and `--assumeyes` answers the rest.
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(subcommand);
        if ctx.assume_yes {
            cmd = cmd.args(["--noninteractive", "--assumeyes"]);
        }
        cmd
    }

    fn no_dry_run(&self, operation: &str) -> Error {
        Error::Other(format!("{ID}: {operation} has no dry-run mode"))
    }
}

/// Flatpak refs name branches, not versions; there is no version pin.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but flatpak only installs a ref's branch head"
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
        "Flatpak"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Universal
    }

    fn database_id(&self) -> &'static str {
        "flatpak"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::REFRESH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        let cmd = self
            .mutation("install", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("uninstall"));
        }
        let cmd = self
            .mutation("uninstall", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["list", "--app", LIST_COLUMNS])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_list(&output.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // `flatpak info` only knows installed refs; anything else is
        // looked up in the remotes' search index by exact application ID.
        let show = self
            .query()
            .arg("info")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if show.success()
            && let Some(package) = parse_info(&show.stdout)
        {
            return Ok(Box::new(package));
        }
        let search = self
            .query()
            .args(["search", SEARCH_COLUMNS])
            .arg(name)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        parse_search(&search.stdout)
            .into_iter()
            .find(|package| package.name.eq_ignore_ascii_case(name))
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["search", SEARCH_COLUMNS])
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_search(&output.stdout)))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("refresh"));
        }
        // The refreshable index is the appstream metadata per remote.
        self.run(self.mutation("update", ctx).arg("--appstream"), ctx)
            .await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        let cmd = self
            .mutation("update", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let updates = self
            .query()
            .args(["remote-ls", "--updates", "--app", UPDATE_COLUMNS])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        let mut outdated = parse_updates(&updates.stdout);
        if outdated.is_empty() {
            return Ok(Vec::new());
        }
        // remote-ls names only the update's version; one list probe fills
        // in what is currently installed.
        let installed = self
            .query()
            .args(["list", "--app", LIST_COLUMNS])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        let installed = parse_list(&installed.stdout);
        for package in &mut outdated {
            if let Some(current) = installed.iter().find(|local| local.name == package.name) {
                package.version = current.version.clone();
                package.description = current.description.clone();
            }
        }
        Ok(boxed(outdated))
    }
}

fn boxed(packages: Vec<FlatpakPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

fn field(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// `flatpak list --app` with application/version/name/origin columns:
/// tab-separated rows; the human-facing name lands in the description slot
/// because flatpak's real identifier is the application ID.
fn parse_list(stdout: &str) -> Vec<FlatpakPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let application = fields.next()?.trim();
            let version = fields.next()?;
            let name = fields.next()?;
            let origin = fields.next()?;
            (!application.is_empty()).then(|| FlatpakPackage {
                name: application.to_string(),
                version: field(version),
                description: field(name),
                origin: field(origin),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `flatpak search` with application/version/remotes/description columns:
/// tab-separated rows; a "No matches found" notice has no tabs and drops
/// out on its own.
fn parse_search(stdout: &str) -> Vec<FlatpakPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(4, '\t');
            let application = fields.next()?.trim();
            let version = fields.next()?;
            let remotes = fields.next()?;
            let description = fields.next()?;
            (!application.is_empty()).then(|| FlatpakPackage {
                name: application.to_string(),
                version: field(version),
                origin: field(remotes),
                description: field(description),
                state: InstallState::Available,
                ..Default::default()
            })
        })
        .collect()
}

/// `flatpak remote-ls --updates` with application/version/origin columns:
/// tab-separated rows naming the version an update would install.
fn parse_updates(stdout: &str) -> Vec<FlatpakPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let application = fields.next()?.trim();
            let version = fields.next()?;
            let origin = fields.next()?;
            (!application.is_empty()).then(|| FlatpakPackage {
                name: application.to_string(),
                latest_version: field(version),
                origin: field(origin),
                state: InstallState::Upgradable,
                ..Default::default()
            })
        })
        .collect()
}

/// `flatpak info`: a `Name - Description` headline followed by
/// right-aligned `Key: Value` fields, so keys are matched after trimming.
fn parse_info(stdout: &str) -> Option<FlatpakPackage> {
    let mut package = FlatpakPackage {
        state: InstallState::Installed,
        ..Default::default()
    };
    let mut headline_pending = true;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if headline_pending {
            headline_pending = false;
            if let Some((title, description)) = line.split_once(" - ")
                && !title.contains(':')
            {
                package.description = field(description);
                continue;
            }
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "ID" => package.name = value.to_string(),
            "Version" => package.version = Some(value.to_string()),
            "License" => package.license = Some(value.to_string()),
            "Arch" => package.architecture = Some(value.to_string()),
            "Origin" => package.origin = Some(value.to_string()),
            "Runtime" => package.dependencies = Some(vec![value.to_string()]),
            _ => {}
        }
    }
    (!package.name.is_empty()).then_some(package)
}

/// A package as flatpak describes it.
#[derive(Debug, Default)]
pub struct FlatpakPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub architecture: Option<String>,
    pub origin: Option<String>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for FlatpakPackage {
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

    fn license(&self) -> Option<&str> {
        self.license.as_deref()
    }

    fn architecture(&self) -> Option<&str> {
        self.architecture.as_deref()
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
    fn parses_installed_list_rows() {
        let stdout = "\
org.gnome.Calculator\t46.1\tCalculator\tflathub
org.mozilla.firefox\t128.0.3\tFirefox\tflathub
com.example.NoVersion\t\tExample\tfedora
";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "org.gnome.Calculator");
        assert_eq!(packages[0].version.as_deref(), Some("46.1"));
        assert_eq!(packages[0].description.as_deref(), Some("Calculator"));
        assert_eq!(packages[0].origin.as_deref(), Some("flathub"));
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[2].version, None);
    }

    #[test]
    fn parses_search_rows_and_skips_no_matches_notice() {
        let stdout = "\
org.gnome.Calculator\t46.1\tflathub\tPerform arithmetic, scientific or financial calculations
No matches found
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "org.gnome.Calculator");
        assert_eq!(packages[0].origin.as_deref(), Some("flathub"));
        assert_eq!(
            packages[0].description.as_deref(),
            Some("Perform arithmetic, scientific or financial calculations")
        );
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn parses_remote_update_rows() {
        let stdout = "org.gnome.Calculator\t46.2\tflathub\n";
        let packages = parse_updates(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].latest_version.as_deref(), Some("46.2"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn parses_info_output() {
        let stdout = "
Calculator - Perform arithmetic, scientific or financial calculations

          ID: org.gnome.Calculator
         Ref: app/org.gnome.Calculator/x86_64/stable
        Arch: x86_64
      Branch: stable
     Version: 46.1
     License: GPL-3.0-or-later
      Origin: flathub
  Collection: org.flathub.Stable
Installation: system
   Installed: 9.6 MB
     Runtime: org.gnome.Platform/x86_64/46
         Sdk: org.gnome.Sdk/x86_64/46
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.name, "org.gnome.Calculator");
        assert_eq!(package.version.as_deref(), Some("46.1"));
        assert_eq!(
            package.description.as_deref(),
            Some("Perform arithmetic, scientific or financial calculations")
        );
        assert_eq!(package.license.as_deref(), Some("GPL-3.0-or-later"));
        assert_eq!(package.architecture.as_deref(), Some("x86_64"));
        assert_eq!(package.origin.as_deref(), Some("flathub"));
        assert_eq!(
            package.dependencies,
            Some(vec!["org.gnome.Platform/x86_64/46".to_string()])
        );
        assert_eq!(package.state, InstallState::Installed);
    }

    #[test]
    fn info_without_headline_still_parses() {
        let stdout = "\
          ID: org.gnome.Calculator
     Version: 46.1
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.name, "org.gnome.Calculator");
        assert_eq!(package.description, None);
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("org.gnome.Calculator@46.1")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("org.gnome.Calculator")]).is_ok());
    }
}
