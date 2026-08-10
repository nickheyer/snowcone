//! sbt backend for snowcone.
//!
//! sbt is a per-project build tool with no package-manager surface -
//! dependencies live in `build.sbt`, so install and remove fail with an
//! explanation rather than fake anything. The reads are real but
//! cwd-scoped: `sbt -batch "show libraryDependencies"` reports the current
//! project's declared dependencies (direct ones only - stock sbt has no
//! guaranteed transitive-listing task). Every read boots a JVM and loads
//! the build, which is slow, so each operation is a single sbt invocation.
//! `-Dsbt.log.noformat=true` strips the log coloring that would otherwise
//! pollute parsing.

use std::collections::BTreeSet;
use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "sbt";
const PROGRAMS: &[&str] = &["sbt"];

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

    /// The current directory's declared dependencies, from one
    /// `show libraryDependencies` run.
    async fn declared_dependencies(&self) -> Result<Vec<SbtPackage>> {
        let output = self
            .query()
            .args([
                "-batch",
                "-Dsbt.log.noformat=true",
                "show libraryDependencies",
            ])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_library_dependencies(&output.stdout))
    }

    fn not_a_package_manager(&self, operation: &str) -> Error {
        Error::Other(format!(
            "{ID}: sbt has no {operation} verb - dependencies are declared in \
             `build.sbt` and resolved at build time"
        ))
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "sbt"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "jvm"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
    }

    async fn install(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        Err(self.not_a_package_manager("install"))
    }

    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        Err(self.not_a_package_manager("remove"))
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .declared_dependencies()
            .await?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // Match a full `org:artifact` coordinate or a bare artifact name.
        self.declared_dependencies()
            .await?
            .into_iter()
            .find(|package| {
                package.name == name || package.name.split(':').nth(1) == Some(name)
            })
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }
}

/// `show libraryDependencies` output: `[info]`-prefixed lines carrying
/// modules either as `* org:artifact:version` bullets (sbt 1.x) or inside
/// a single `List(org:a:1, org:b:2)` (older sbt), each module optionally
/// suffixed with a `:configuration` like `:test`; anything else on the
/// `[info]` stream is loading noise.
fn parse_library_dependencies(stdout: &str) -> Vec<SbtPackage> {
    let mut seen = BTreeSet::new();
    let mut packages = Vec::new();
    let mut add = |text: &str| {
        let Some((name, version)) = split_module(text) else {
            return;
        };
        if seen.insert((name.clone(), version.clone())) {
            packages.push(SbtPackage {
                name,
                version,
                state: InstallState::Installed,
            });
        }
    };
    for line in stdout.lines() {
        let Some(rest) = line.strip_prefix("[info]") else {
            continue;
        };
        let rest = rest.trim();
        if let Some(bullet) = rest.strip_prefix("* ") {
            add(bullet.trim());
        } else if let Some(start) = rest.find("List(") {
            let inner = &rest[start + 5..];
            let Some(end) = inner.rfind(')') else {
                continue;
            };
            for module in inner[..end].split(',') {
                add(module.trim());
            }
        }
    }
    packages
}

/// `org:artifact:version[:configuration]` as sbt prints a ModuleID; all
/// three leading segments are required, which filters out loading noise.
fn split_module(text: &str) -> Option<(String, Option<String>)> {
    let mut segments = text.split(':');
    let (org, artifact, version) = (segments.next()?, segments.next()?, segments.next()?);
    let clean = |segment: &str| !segment.is_empty() && !segment.contains(char::is_whitespace);
    if !clean(org) || !clean(artifact) || !clean(version) {
        return None;
    }
    Some((format!("{org}:{artifact}"), Some(version.to_string())))
}

/// A declared dependency as `show libraryDependencies` describes it.
#[derive(Debug)]
pub struct SbtPackage {
    /// `org:artifact` coordinate.
    pub name: String,
    pub version: Option<String>,
    pub state: InstallState,
}

impl Package for SbtPackage {
    fn manager(&self) -> &str {
        ID
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bulleted_show_output() {
        let stdout = "\
[info] welcome to sbt 1.9.8 (Eclipse Adoptium Java 17.0.9)
[info] loading project definition from /home/nick/demo/project
[info] loading settings for project demo from build.sbt ...
[info] * org.scala-lang:scala-library:2.13.12
[info] * org.typelevel:cats-core:2.10.0
[info] * org.scalatest:scalatest:3.2.17:test
[success] Total time: 1 s, completed Aug 9, 2026
";
        let packages = parse_library_dependencies(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "org.scala-lang:scala-library");
        assert_eq!(packages[0].version.as_deref(), Some("2.13.12"));
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[2].name, "org.scalatest:scalatest");
        assert_eq!(packages[2].version.as_deref(), Some("3.2.17"));
    }

    #[test]
    fn parses_list_show_output() {
        let stdout = "\
[info] Set current project to demo
[info] List(org.scala-lang:scala-library:2.13.12, com.typesafe:config:1.4.3)
";
        let packages = parse_library_dependencies(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "com.typesafe:config");
        assert_eq!(packages[1].version.as_deref(), Some("1.4.3"));
    }

    #[test]
    fn splits_modules_and_rejects_noise() {
        assert_eq!(
            split_module("org.typelevel:cats-core:2.10.0"),
            Some(("org.typelevel:cats-core".to_string(), Some("2.10.0".to_string())))
        );
        assert_eq!(
            split_module("org.scalatest:scalatest:3.2.17:test"),
            Some(("org.scalatest:scalatest".to_string(), Some("3.2.17".to_string())))
        );
        assert_eq!(split_module("compiling 3 Scala sources"), None);
        assert_eq!(split_module("done: total time"), None);
    }
}
