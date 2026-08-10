//! Composer backend for snowcone.
//!
//! Manages Composer's global scope (`composer global …`) - per-project
//! `vendor/` trees belong to project tooling, not a system package CLI.
//! Listings speak JSON via `show --format=json`; search and the registry
//! view (`show --all`) are stable text. Composer prompts only rarely
//! (plugin trust questions and the like), so `assume_yes` simply maps to
//! `--no-interaction`. `--dry-run` is native to require, remove, and
//! update, though require/remove only learned it in Composer 2.4.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "composer";
const PROGRAMS: &[&str] = &["composer"];

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

    /// Shared flags for mutating `global` subcommands: `--no-interaction`
    /// and the native dry run.
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg("global").arg(subcommand);
        if ctx.assume_yes {
            cmd = cmd.arg("--no-interaction");
        }
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        cmd
    }

    /// Globally installed packages via `global show --format=json`. A
    /// COMPOSER_HOME that has never seen a `global require` has no
    /// composer.json for `show` to read - that simply means nothing is
    /// installed.
    async fn global_show(&self, outdated: bool) -> Result<Vec<ComposerPackage>> {
        let mut cmd = self.cmd().args(["global", "show", "--format=json"]);
        if outdated {
            cmd = cmd.arg("--outdated");
        }
        let output = cmd.capture(&self.elevator, None).await?;
        if let Ok(json) = serde_json::from_str::<Value>(&output.stdout) {
            return Ok(parse_show_json(&json));
        }
        if !output.success() && output.stderr.contains("composer.json") {
            return Ok(Vec::new());
        }
        output.require_success()?;
        Ok(Vec::new())
    }
}

/// `vendor/name:version` when the request pins one, bare name otherwise.
fn spec(request: &PackageRequest) -> String {
    match &request.version {
        Some(version) => format!("{}:{version}", request.name),
        None => request.name.clone(),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Composer"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "composer"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = self
            .mutation("require", ctx)
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
        Ok(boxed(self.global_show(false).await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query()
            .args(["global", "show", "--all"])
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        let mut package =
            parse_show_text(&output.stdout).ok_or_else(|| Error::NotFound(name.to_string()))?;
        // The registry view says nothing about the global install.
        if let Some(installed) = self
            .global_show(false)
            .await?
            .into_iter()
            .find(|installed| installed.name == package.name)
        {
            package.state = InstallState::Installed;
            if installed.version.is_some() && installed.version != package.version {
                package.latest_version = package.version.take();
                package.version = installed.version;
            }
        }
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["global", "search"])
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_search(&output.stdout)))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = if packages.is_empty() {
            self.mutation("update", ctx)
        } else {
            // Re-requiring resolves a fresh constraint, so a targeted
            // upgrade can cross the caret range recorded at install time
            // (and carries pins as `name:version`); `update` would stay
            // inside it.
            self.mutation("require", ctx).args(packages.iter().map(spec))
        };
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.global_show(true).await?))
    }
}

fn boxed(packages: Vec<ComposerPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `composer global show --format=json`: an `installed` array of
/// `{name, version, description}` entries, gaining `latest` under
/// `--outdated`.
fn parse_show_json(json: &Value) -> Vec<ComposerPackage> {
    let Some(entries) = json["installed"].as_array() else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let latest = entry["latest"].as_str().map(str::to_string);
            Some(ComposerPackage {
                name: entry["name"].as_str()?.to_string(),
                version: entry["version"].as_str().map(str::to_string),
                description: entry["description"].as_str().map(str::to_string),
                state: if latest.is_some() {
                    InstallState::Upgradable
                } else {
                    InstallState::Installed
                },
                latest_version: latest,
                ..Default::default()
            })
        })
        .collect()
}

/// `composer search`: one `vendor/name description…` line per hit; the
/// description may be absent.
fn parse_search(stdout: &str) -> Vec<ComposerPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, char::is_whitespace);
            let name = parts.next()?;
            if !name.contains('/') {
                return None;
            }
            Some(ComposerPackage {
                name: name.to_string(),
                description: parts
                    .next()
                    .map(str::trim)
                    .filter(|description| !description.is_empty())
                    .map(str::to_string),
                state: InstallState::Available,
                ..Default::default()
            })
        })
        .collect()
}

/// `composer show --all`: a `key : value` header block (`descrip.` is the
/// summary), then bare section headers (`requires`, `autoload`, …) whose
/// rows are `name constraint` pairs.
fn parse_show_text(stdout: &str) -> Option<ComposerPackage> {
    let mut package = ComposerPackage {
        state: InstallState::Available,
        ..Default::default()
    };
    let mut in_requires = false;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "requires" {
            in_requires = true;
            package.dependencies.get_or_insert_with(Vec::new);
            continue;
        }
        if let Some((key, value)) = line.split_once(':')
            && !key.trim().is_empty()
            && !key.trim().contains(' ')
        {
            in_requires = false;
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            match key.trim() {
                "name" => package.name = value.to_string(),
                "descrip." => package.description = Some(value.to_string()),
                "homepage" => package.homepage = Some(value.to_string()),
                "license" => package.license = Some(value.to_string()),
                "versions" => package.version = pick_version(value),
                _ => {}
            }
            continue;
        }
        if in_requires {
            match trimmed
                .split_whitespace()
                .next()
                .filter(|dep| is_dependency_name(dep))
            {
                Some(dep) => {
                    if let Some(dependencies) = &mut package.dependencies {
                        dependencies.push(dep.to_string());
                    }
                }
                // `requires (dev)` and friends end the section.
                None => in_requires = false,
            }
        }
    }
    (!package.name.is_empty()).then_some(package)
}

/// First released entry on a `versions` line - dev branches (`dev-main`,
/// `3.x-dev`) sort ahead of releases, and an installed version carries a
/// `* ` marker.
fn pick_version(list: &str) -> Option<String> {
    let entries: Vec<&str> = list
        .split(',')
        .map(|entry| entry.trim().trim_start_matches('*').trim_start())
        .filter(|entry| !entry.is_empty())
        .collect();
    entries
        .iter()
        .find(|entry| !entry.contains("dev"))
        .or_else(|| entries.first())
        .copied()
        .map(str::to_string)
}

/// Composer dependency names: `vendor/package` plus the platform packages
/// (`php`, `ext-*`, `lib-*`, `composer-*`).
fn is_dependency_name(token: &str) -> bool {
    token.contains('/')
        || token == "php"
        || token.starts_with("php-")
        || token.starts_with("ext-")
        || token.starts_with("lib-")
        || token.starts_with("composer-")
}

/// A package as composer describes it.
#[derive(Debug, Default)]
pub struct ComposerPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for ComposerPackage {
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
    fn parses_installed_show_json() {
        let json: Value = serde_json::from_str(
            r#"{"installed": [
                {"name": "friendsofphp/php-cs-fixer", "version": "v3.64.0",
                 "description": "A tool to automatically fix PHP code style"},
                {"name": "psy/psysh", "version": "v0.12.4",
                 "description": "An interactive shell for modern PHP."}
            ]}"#,
        )
        .unwrap();
        let packages = parse_show_json(&json);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "friendsofphp/php-cs-fixer");
        assert_eq!(packages[0].version.as_deref(), Some("v3.64.0"));
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[0].latest_version, None);
    }

    #[test]
    fn parses_outdated_show_json() {
        let json: Value = serde_json::from_str(
            r#"{"installed": [
                {"name": "psy/psysh", "version": "v0.12.0", "latest": "v0.12.4",
                 "latest-status": "semver-safe-update",
                 "description": "An interactive shell for modern PHP."}
            ]}"#,
        )
        .unwrap();
        let packages = parse_show_json(&json);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].version.as_deref(), Some("v0.12.0"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("v0.12.4"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn parses_search_lines() {
        let stdout = "\
monolog/monolog Sends your logs to files, sockets, inboxes, databases and various web services
seldaek/monolog-bridge
Some warning without a package name
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "monolog/monolog");
        assert!(
            packages[0]
                .description
                .as_deref()
                .unwrap()
                .starts_with("Sends your logs")
        );
        assert_eq!(packages[1].description, None);
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn parses_show_all_text() {
        let stdout = "\
name     : monolog/monolog
descrip. : Sends your logs to files, sockets, inboxes, databases and various web services
keywords : log, logging, psr-3
versions : dev-main, 3.x-dev, 3.7.0, 3.6.0, 2.9.3
type     : library
license  : MIT License (MIT) (OSI approved) https://spdx.org/licenses/MIT.html
homepage : https://github.com/Seldaek/monolog
source   : [git] https://github.com/Seldaek/monolog.git 4b1cf9c
names    : monolog/monolog, psr/log-implementation

requires
php >=8.1
psr/log ^2.0 || ^3.0

requires (dev)
phpunit/phpunit ^10.5.17
";
        let package = parse_show_text(stdout).unwrap();
        assert_eq!(package.name, "monolog/monolog");
        assert_eq!(package.version.as_deref(), Some("3.7.0"));
        assert!(
            package
                .description
                .as_deref()
                .unwrap()
                .starts_with("Sends your logs")
        );
        assert_eq!(
            package.homepage.as_deref(),
            Some("https://github.com/Seldaek/monolog")
        );
        assert_eq!(
            package.dependencies,
            Some(vec!["php".to_string(), "psr/log".to_string()])
        );
        assert_eq!(package.state, InstallState::Available);
    }

    #[test]
    fn picks_installed_marker_version() {
        assert_eq!(pick_version("* v3.64.0").as_deref(), Some("v3.64.0"));
        assert_eq!(
            pick_version("dev-main, 9999999-dev").as_deref(),
            Some("dev-main")
        );
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("monolog/monolog@3.7.0")),
            "monolog/monolog:3.7.0"
        );
        assert_eq!(
            spec(&PackageRequest::parse("monolog/monolog")),
            "monolog/monolog"
        );
    }
}
