//! Gradle backend for snowcone.
//!
//! Gradle is a per-project build tool with no package-manager surface: it
//! has no verb to install or remove an artifact imperatively - dependencies
//! are declared in the project's build script and resolved at build time -
//! so install and remove fail with an explanation rather than fake
//! anything. The two reads are real but cwd-scoped: they run
//! `gradle dependencies` against the Gradle project in the current
//! directory (failing with Gradle's own error outside one) and parse the
//! resolved dependency tree across every configuration it prints.
//! `--console=plain` keeps the rich console's redrawing out of the output.

use std::collections::BTreeSet;
use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "gradle";
const PROGRAMS: &[&str] = &["gradle"];

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

    /// The current directory's project dependencies, from
    /// `gradle dependencies`.
    async fn project_dependencies(&self) -> Result<Vec<GradlePackage>> {
        let output = self
            .query()
            .args(["-q", "--console=plain", "dependencies"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_dependency_tree(&output.stdout))
    }

    fn not_a_package_manager(&self, operation: &str) -> Error {
        Error::Other(format!(
            "{ID}: gradle has no {operation} verb - dependencies are declared in the \
             project's build script and resolved at build time"
        ))
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Gradle"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "jvm"
    }

    fn capabilities(&self) -> Capabilities {
        // No INSTALL or REMOVE: gradle has no imperative package verbs, so
        // those bits are dropped and the methods explain why.
        Capabilities::LIST_INSTALLED | Capabilities::INFO
    }

    async fn install(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        Err(self.not_a_package_manager("install"))
    }

    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        Err(self.not_a_package_manager("remove"))
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .project_dependencies()
            .await?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // Match a full `group:artifact` coordinate or a bare artifact name.
        self.project_dependencies()
            .await?
            .into_iter()
            .find(|package| package.name == name || package.name.split(':').nth(1) == Some(name))
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }
}

/// `gradle dependencies` tree lines: `+--- group:artifact:version` (also
/// `\---`, nested under `|    ` guides), where a ` -> ` arrow names what
/// actually resolved, `project :x` entries are sibling projects, and
/// `(*)`/`(c)`/`(n)` markers are noise; configurations repeat dependencies,
/// so entries are deduplicated.
fn parse_dependency_tree(stdout: &str) -> Vec<GradlePackage> {
    let mut seen = BTreeSet::new();
    let mut packages = Vec::new();
    for line in stdout.lines() {
        let Some(position) = line.find("--- ") else {
            continue;
        };
        let entry = &line[position + 4..];
        if entry.starts_with("project ") {
            continue;
        }
        let mut tokens = entry.split_whitespace();
        let Some(coordinate) = tokens.next() else {
            continue;
        };
        let resolved = tokens
            .next()
            .filter(|token| *token == "->")
            .and_then(|_| tokens.next());
        let Some((name, version)) = split_coordinate(coordinate, resolved) else {
            continue;
        };
        if seen.insert((name.clone(), version.clone())) {
            packages.push(GradlePackage {
                name,
                version,
                state: InstallState::Installed,
            });
        }
    }
    packages
}

/// `group:artifact[:version]`, with an optional `-> resolved` override that
/// is either a bare version or a whole replacement coordinate.
fn split_coordinate(coordinate: &str, resolved: Option<&str>) -> Option<(String, Option<String>)> {
    // A replacement coordinate after the arrow wins wholesale.
    if let Some(resolved) = resolved
        && resolved.contains(':')
    {
        return split_coordinate(resolved, None);
    }
    let mut segments = coordinate.split(':');
    let (group, artifact) = (segments.next()?, segments.next()?);
    if group.is_empty() || artifact.is_empty() {
        return None;
    }
    let version = resolved
        .map(str::to_string)
        .or_else(|| segments.next().map(str::to_string))
        .filter(|version| !version.is_empty() && !version.starts_with(['{', '(']));
    Some((format!("{group}:{artifact}"), version))
}

/// A dependency as `gradle dependencies` describes it.
#[derive(Debug)]
pub struct GradlePackage {
    /// `group:artifact` coordinate.
    pub name: String,
    pub version: Option<String>,
    pub state: InstallState,
}

impl Package for GradlePackage {
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
    fn parses_dependency_tree() {
        let stdout = "\
------------------------------------------------------------
Root project 'demo'
------------------------------------------------------------

compileClasspath - Compile classpath for source set 'main'.
+--- org.apache.commons:commons-lang3:3.14.0
+--- com.google.guava:guava:33.0.0-jre
|    +--- com.google.guava:failureaccess:1.2.0
|    \\--- org.checkerframework:checker-qual:3.41.0
\\--- project :shared

runtimeClasspath - Runtime classpath of source set 'main'.
+--- org.apache.commons:commons-lang3:3.14.0
\\--- com.google.guava:guava:33.0.0-jre (*)
";
        let packages = parse_dependency_tree(stdout);
        assert_eq!(packages.len(), 4);
        assert_eq!(packages[0].name, "org.apache.commons:commons-lang3");
        assert_eq!(packages[0].version.as_deref(), Some("3.14.0"));
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[2].name, "com.google.guava:failureaccess");
    }

    #[test]
    fn resolves_version_conflict_arrows() {
        let stdout = "\
+--- org.slf4j:slf4j-api:1.7.36 -> 2.0.12
+--- com.example:old -> com.example:new:2.0 (*)
";
        let packages = parse_dependency_tree(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].version.as_deref(), Some("2.0.12"));
        assert_eq!(packages[1].name, "com.example:new");
        assert_eq!(packages[1].version.as_deref(), Some("2.0"));
    }

    #[test]
    fn splits_coordinates() {
        assert_eq!(
            split_coordinate("org.slf4j:slf4j-api:2.0.12", None),
            Some((
                "org.slf4j:slf4j-api".to_string(),
                Some("2.0.12".to_string())
            ))
        );
        assert_eq!(
            split_coordinate("org.slf4j:slf4j-api", Some("2.0.12")),
            Some((
                "org.slf4j:slf4j-api".to_string(),
                Some("2.0.12".to_string())
            ))
        );
        assert_eq!(split_coordinate("slf4j-api", None), None);
    }
}
