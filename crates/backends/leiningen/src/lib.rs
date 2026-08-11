//! Leiningen backend for snowcone.
//!
//! `lein` is a project build tool, not a system package manager, so this
//! backend is explicitly cwd-scoped: install, list-installed, and the
//! installed half of info operate on the Leiningen project in the current
//! directory (its `project.clj`), exactly as running `lein` there would.
//! Search is the one global verb - `lein search` queries Clojars and Maven
//! Central. Install uses lein's documented `change` task to append the
//! dependency (as the Maven `RELEASE` meta-version, since coordinates
//! require one) and `deps` to fetch it; lein has no verb that removes a
//! dependency, so remove always errors. lein never prompts and nothing
//! needs root.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "leiningen";
const PROGRAMS: &[&str] = &["lein"];

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

    /// The cwd project's resolved dependency tree. lein releases have moved
    /// the tree between stdout and stderr, so whichever stream yields
    /// entries wins.
    async fn deps_tree(&self) -> Result<Vec<LeiningenPackage>> {
        let output = self
            .query()
            .args(["deps", ":tree"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        let packages = parse_tree(&output.stdout);
        if packages.is_empty() {
            return Ok(parse_tree(&output.stderr));
        }
        Ok(packages)
    }

    async fn search_registries(&self, query: &str) -> Result<Vec<LeiningenPackage>> {
        let output = self
            .query()
            .arg("search")
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_search(&output.stdout))
    }
}

/// Dependencies are appended as the newest release; an explicit pin has no
/// spelling here without `PIN_VERSION`.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but this backend always adds the newest release"
        ))),
        None => Ok(()),
    }
}

/// The EDN vector `lein change :dependencies conj` expects; `RELEASE` is
/// the Maven meta-version resolving to the newest stable release.
fn dependency_vector(name: &str) -> String {
    format!("[{name} \"RELEASE\"]")
}

/// Exact match on a printed artifact name, or on its artifact half when the
/// request omits the group (`core.async` matches `org.clojure/core.async`).
fn matches_name(printed: &str, requested: &str) -> bool {
    printed == requested
        || (!requested.contains('/')
            && printed
                .rsplit_once('/')
                .is_some_and(|(_, artifact)| artifact == requested))
}

/// `name "version" …` as printed inside a dependency vector; the version is
/// the first double-quoted token, anything after it (`:exclusions`, …) is
/// noise.
fn parse_coordinates(inner: &str) -> Option<(String, Option<String>)> {
    let name = inner.split_whitespace().next()?.to_string();
    let version = inner.split('"').nth(1).map(str::to_string);
    Some((name, version))
}

/// `lein search`: `[name "version"]` headers between `Searching …` banner
/// lines - the description sits indented on the following line on current
/// lein, trailing on the same line on old lein.
fn parse_search(stdout: &str) -> Vec<LeiningenPackage> {
    let mut packages: Vec<LeiningenPackage> = Vec::new();
    for line in stdout.lines() {
        if line.starts_with(char::is_whitespace) {
            if let (Some(last), text) = (packages.last_mut(), line.trim())
                && !text.is_empty()
                && last.description.is_none()
            {
                last.description = Some(text.to_string());
            }
            continue;
        }
        let Some(rest) = line.strip_prefix('[') else {
            continue;
        };
        let Some((inner, trailing)) = rest.split_once(']') else {
            continue;
        };
        let Some((name, version)) = parse_coordinates(inner) else {
            continue;
        };
        let mut package = LeiningenPackage {
            name,
            version,
            state: InstallState::Available,
            ..Default::default()
        };
        let trailing = trailing.trim();
        if !trailing.is_empty() {
            package.description = Some(trailing.to_string());
        }
        packages.push(package);
    }
    packages
}

/// `lein deps :tree`: one indented `[name "version" …]` vector per resolved
/// artifact (nesting only deepens the indent); unindented lines are banners
/// and "possibly confusing dependencies" warnings.
fn parse_tree(output: &str) -> Vec<LeiningenPackage> {
    output
        .lines()
        .filter_map(|line| {
            if !line.starts_with(char::is_whitespace) {
                return None;
            }
            let inner = line.trim().strip_prefix('[')?;
            let (name, version) = parse_coordinates(inner)?;
            Some(LeiningenPackage {
                name,
                version,
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Leiningen"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "jvm"
    }

    fn capabilities(&self) -> Capabilities {
        // No REMOVE: lein has no verb that removes a dependency, so the
        // bit is dropped and the method explains the manual workflow.
        Capabilities::INSTALL
            | Capabilities::LIST_INSTALLED
            | Capabilities::INFO
            | Capabilities::SEARCH
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: install has no dry-run mode")));
        }
        // `lein change` only rewrites project.clj; `lein deps` makes the
        // new entries real by resolving and downloading them.
        for package in packages {
            let cmd = self
                .cmd()
                .args(["change", ":dependencies", "conj"])
                .arg(dependency_vector(&package.name));
            self.run(cmd, ctx).await?;
        }
        self.run(self.cmd().arg("deps"), ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        let names: Vec<&str> = packages
            .iter()
            .map(|package| package.name.as_str())
            .collect();
        Err(Error::Other(format!(
            "{ID}: lein has no verb that removes a dependency; delete {} from project.clj by hand",
            names.join(", ")
        )))
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .deps_tree()
            .await?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let found = self
            .search_registries(name)
            .await?
            .into_iter()
            .find(|package| matches_name(&package.name, name));
        // The registry says nothing about the cwd project; outside a
        // Leiningen project the local half simply has nothing installed.
        let installed = match self.deps_tree().await {
            Ok(tree) => tree
                .into_iter()
                .find(|package| matches_name(&package.name, name)),
            Err(_) => None,
        };
        match (found, installed) {
            (Some(mut package), Some(local)) => {
                package.state = InstallState::Installed;
                if local.version.is_some() && local.version != package.version {
                    package.latest_version = package.version.take();
                    package.version = local.version;
                }
                Ok(Box::new(package))
            }
            (Some(package), None) => Ok(Box::new(package)),
            (None, Some(local)) => Ok(Box::new(local)),
            (None, None) => Err(Error::NotFound(name.to_string())),
        }
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .search_registries(query)
            .await?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }
}

/// A package as lein describes it.
#[derive(Debug, Default)]
pub struct LeiningenPackage {
    /// Artifact name, group-qualified when the group differs from it.
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for LeiningenPackage {
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

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search_results() {
        let stdout = "\
Searching clojars ...
[clj-http \"3.12.3\"]
  HTTP library wrapping Apache HttpComponents client.
[org.clojure/clojure \"1.11.1\"]
  Clojure core environment and runtime library.
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "clj-http");
        assert_eq!(packages[0].version.as_deref(), Some("3.12.3"));
        assert_eq!(
            packages[0].description.as_deref(),
            Some("HTTP library wrapping Apache HttpComponents client.")
        );
        assert_eq!(packages[1].name, "org.clojure/clojure");
        assert_eq!(packages[1].state, InstallState::Available);
    }

    #[test]
    fn parses_old_style_same_line_descriptions() {
        let packages = parse_search("[lein-ring \"0.12.5\"] Leiningen Ring plugin\n");
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].version.as_deref(), Some("0.12.5"));
        assert_eq!(
            packages[0].description.as_deref(),
            Some("Leiningen Ring plugin")
        );
    }

    #[test]
    fn parses_deps_tree() {
        let output = "\
Possibly confusing dependencies found:
[clj-http \"2.0.0\"] -> [potemkin \"0.4.1\"]
 overrides
[midje \"1.8.3\"] -> [potemkin \"0.4.0\"]

 [cheshire \"5.10.0\"]
   [com.fasterxml.jackson.core/jackson-core \"2.10.2\"]
 [clj-http \"3.12.3\" :exclusions [commons-logging]]
 [org.clojure/clojure \"1.11.1\"]
   [org.clojure/core.specs.alpha \"0.2.62\"]
";
        let packages = parse_tree(output);
        assert_eq!(packages.len(), 5);
        assert_eq!(packages[0].name, "cheshire");
        assert_eq!(packages[0].version.as_deref(), Some("5.10.0"));
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[2].name, "clj-http");
        assert_eq!(packages[2].version.as_deref(), Some("3.12.3"));
        assert_eq!(packages[4].name, "org.clojure/core.specs.alpha");
    }

    #[test]
    fn matches_bare_and_qualified_names() {
        assert!(matches_name("org.clojure/core.async", "core.async"));
        assert!(matches_name(
            "org.clojure/core.async",
            "org.clojure/core.async"
        ));
        assert!(matches_name("clj-http", "clj-http"));
        assert!(!matches_name("core.async", "org.clojure/core.async"));
        assert!(!matches_name("org.clojure/core.async", "async"));
    }

    #[test]
    fn formats_dependency_vectors() {
        assert_eq!(dependency_vector("clj-http"), "[clj-http \"RELEASE\"]");
        assert_eq!(
            dependency_vector("org.clojure/core.async"),
            "[org.clojure/core.async \"RELEASE\"]"
        );
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("clj-http@3.12.3")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("clj-http")]).is_ok());
    }
}
