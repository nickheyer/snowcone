//! AppImage backend for snowcone, driving ivan-hc's `am`.
//!
//! Discovery also recognizes `zap` and `appimaged`, but only `am` speaks
//! the full manager verb set this backend needs, so operations on the
//! other two explain what to install instead of guessing at their CLIs.
//! `am` escalates itself through sudo for its /opt installs - snowcone
//! never prefixes an elevation helper but still reports the coming
//! credential prompt. AM draws its tables with `\u{25c6}` bullets, `|`
//! separators and ANSI colors, so parsers strip escapes first. No verb
//! simulates, so `--dry-run` always errors, and AM has no update-check
//! verb at all, so the outdated listing is not advertised.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "appimage";
const PROGRAMS: &[&str] = &["am", "zap", "appimaged"];

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
                reason: format!("none of {PROGRAMS:?} found on PATH"),
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

    fn no_dry_run(&self, operation: &str) -> Error {
        Error::Other(format!("{ID}: {operation} has no dry-run mode"))
    }

    /// Discovery may have matched a neighbor tool that this backend cannot
    /// operate; only `am` has install/remove/query/update verbs.
    fn require_am(&self) -> Result<()> {
        if self.program.file_name().is_some_and(|name| name == "am") {
            return Ok(());
        }
        Err(Error::Other(format!(
            "{ID}: found `{}`, but this backend drives `am` (ivan-hc/AM) - install `am` to manage AppImages through snowcone",
            self.program.display()
        )))
    }

    /// Installed programs from `am -f`.
    async fn files(&self) -> Result<Vec<AppimagePackage>> {
        let output = self
            .query()
            .arg("-f")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_files(&output.stdout))
    }

    /// Database hits from `am -q`. A no-match run may exit non-zero with
    /// nothing usable on stdout - that is an empty result, not a failure.
    async fn database(&self, keyword: &str) -> Result<Vec<AppimagePackage>> {
        let output = self
            .query()
            .arg("-q")
            .arg(keyword)
            .capture(&self.elevator, None)
            .await?;
        if output.success() || !output.stdout.trim().is_empty() {
            return Ok(parse_query(&output.stdout));
        }
        Ok(Vec::new())
    }
}

/// AM's database installs each app's latest release; nothing pins.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but AM only installs an app's latest release"
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
        "AppImage"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Universal
    }

    fn database_id(&self) -> &'static str {
        "appimage"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::SEARCH | Capabilities::UPGRADE
    }

    /// `am` drives sudo itself for its /opt installs - snowcone never
    /// elevates it, but a credential prompt is still coming.
    fn needs_elevation(&self, operation: Operation) -> bool {
        operation.mutates()
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        self.require_am()?;
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        for package in packages {
            self.run(self.cmd().arg("-i").arg(&package.name), ctx)
                .await?;
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        self.require_am()?;
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        // `-R` skips AM's own confirmation prompt, `-r` keeps it.
        let flag = if ctx.assume_yes { "-R" } else { "-r" };
        for package in packages {
            self.run(self.cmd().arg(flag).arg(&package.name), ctx)
                .await?;
        }
        Ok(())
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        self.require_am()?;
        Ok(boxed(self.files().await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        self.require_am()?;
        let installed = self
            .files()
            .await?
            .into_iter()
            .find(|package| package.name == name);
        // The install table has no descriptions; the database query does.
        let described = self
            .database(name)
            .await?
            .into_iter()
            .find(|package| package.name == name);
        match (installed, described) {
            (Some(mut package), described) => {
                package.description = described.and_then(|hit| hit.description);
                Ok(Box::new(package))
            }
            (None, Some(package)) => Ok(Box::new(package)),
            (None, None) => Err(Error::NotFound(name.to_string())),
        }
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        self.require_am()?;
        Ok(boxed(self.database(query).await?))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        self.require_am()?;
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        if packages.is_empty() {
            return self.run(self.cmd().arg("-u"), ctx).await;
        }
        for package in packages {
            self.run(self.cmd().arg("-u").arg(&package.name), ctx)
                .await?;
        }
        Ok(())
    }
}

fn boxed(packages: Vec<AppimagePackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// Drop ANSI CSI sequences; AM colors its tables even when piped.
fn strip_ansi(text: &str) -> String {
    let mut cleaned = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            cleaned.push(c);
            continue;
        }
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    cleaned
}

/// `am -f`: an `- APPNAME | VERSION | TYPE | SIZE` table (a DB column
/// slips in ahead of VERSION when third-party lists are enabled) with one
/// `\u{25c6}`-bulleted row per installed program.
fn parse_files(stdout: &str) -> Vec<AppimagePackage> {
    let stdout = strip_ansi(stdout);
    let has_db_column = stdout
        .lines()
        .any(|line| line.contains("APPNAME") && line.contains("| DB |"));
    stdout
        .lines()
        .filter_map(|line| {
            let row = line.trim().strip_prefix('\u{25c6}')?;
            let mut fields = row.split('|').map(str::trim);
            let name = fields.next()?;
            if has_db_column {
                fields.next()?;
            }
            let version = fields.next()?;
            (!name.is_empty()).then(|| AppimagePackage {
                name: name.to_string(),
                version: (!version.is_empty()).then(|| version.to_string()),
                description: None,
                state: InstallState::Installed,
            })
        })
        .collect()
}

/// `am -q`: one `\u{25c6} name : description` line per database hit.
fn parse_query(stdout: &str) -> Vec<AppimagePackage> {
    let stdout = strip_ansi(stdout);
    stdout
        .lines()
        .filter_map(|line| {
            let row = line.trim().strip_prefix('\u{25c6}')?.trim();
            let (name, description) = match row.split_once(':') {
                Some((name, description)) => (name.trim(), description.trim()),
                None => (row, ""),
            };
            (!name.is_empty()).then(|| AppimagePackage {
                name: name.to_string(),
                version: None,
                description: (!description.is_empty()).then(|| description.to_string()),
                state: InstallState::Available,
            })
        })
        .collect()
}

/// A package as AM describes it.
#[derive(Debug, Default)]
pub struct AppimagePackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for AppimagePackage {
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

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_sequences() {
        assert_eq!(
            strip_ansi("\u{1b}[32m\u{25c6} firefox\u{1b}[0m | 128.0"),
            "\u{25c6} firefox | 128.0"
        );
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn parses_files_table() {
        let stdout = "\
- APPNAME | VERSION | TYPE | SIZE
- ------- | ------- | ---- | ----
 \u{25c6} am | 9.3 | appimage | 4.7 MB
 \u{25c6} firefox | 128.0.3 | appimage | 256 MB
";
        let packages = parse_files(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "firefox");
        assert_eq!(packages[1].version.as_deref(), Some("128.0.3"));
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn parses_files_table_with_db_column() {
        let stdout = "\
- APPNAME | DB | VERSION | TYPE | SIZE
 \u{25c6} obsidian | am | 1.6.7 | appimage | 512 MB
";
        let packages = parse_files(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "obsidian");
        assert_eq!(packages[0].version.as_deref(), Some("1.6.7"));
    }

    #[test]
    fn parses_colored_files_table() {
        let stdout = " \u{1b}[32m\u{25c6} firefox\u{1b}[0m | 128.0.3 | appimage | 256 MB\n";
        let packages = parse_files(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "firefox");
    }

    #[test]
    fn parses_query_results() {
        let stdout = "\
\u{25c6} firefox : Mozilla Firefox web browser.
  \u{25c6} firefox-esr : Extended Support Release.
Some footer line without a bullet
";
        let packages = parse_query(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "firefox");
        assert_eq!(
            packages[0].description.as_deref(),
            Some("Mozilla Firefox web browser.")
        );
        assert_eq!(packages[1].name, "firefox-esr");
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("firefox@128.0")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("firefox")]).is_ok());
    }
}
