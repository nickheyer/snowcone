//! Maven backend for snowcone.
//!
//! Maven is a build tool, not an app manager: the only package database it
//! owns is the local repository (`~/.m2/repository` by default), and that
//! is what this backend manages. Package names are Maven coordinates -
//! `group:artifact` for remove/info matching, and a full
//! `group:artifact:version` for install, because `dependency:get` cannot
//! fetch an unversioned artifact. Maven has no remote search or metadata
//! verb, so info() and list-installed read the local repository layout
//! directly (the path comes from `help:evaluate`). Every invocation passes
//! `-B` (batch mode), under which mvn never prompts.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "maven";
const PROGRAMS: &[&str] = &["mvn"];

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
        Cmd::new(&self.program).arg("-B")
    }

    /// Read invocation with a stable locale and `-q`, so only the asked-for
    /// value reaches stdout.
    fn query(&self) -> Cmd {
        Cmd::new(&self.program)
            .args(["-B", "-q"])
            .env("LC_ALL", "C")
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

    /// The local repository path, from the effective settings.
    async fn local_repository(&self) -> Result<PathBuf> {
        let output = self
            .query()
            .args([
                "help:evaluate",
                "-Dexpression=settings.localRepository",
                "-DforceStdout",
            ])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        let path = output.stdout.trim();
        if path.is_empty() {
            return Err(Error::Other(format!(
                "{ID}: could not determine the local repository path"
            )));
        }
        Ok(PathBuf::from(path))
    }
}

/// Maven coordinates carry their version inside the name; the `@version`
/// spelling has no Maven syntax to translate to.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version with `@`; spell it inside the \
             coordinate instead (`group:artifact:version`)"
        ))),
        None => Ok(()),
    }
}

/// Install needs a full `group:artifact:version` coordinate (optionally
/// `:packaging[:classifier]` too), because `dependency:get` cannot resolve
/// an unversioned artifact.
fn install_coordinate(name: &str) -> Result<&str> {
    let segments = name.split(':').count();
    if (3..=5).contains(&segments) && name.split(':').all(|segment| !segment.is_empty()) {
        Ok(name)
    } else {
        Err(Error::Other(format!(
            "{ID}: `{name}` is not a full `group:artifact:version` coordinate, \
             which `dependency:get` requires"
        )))
    }
}

/// The `group:artifact` prefix of a coordinate; trailing
/// `:version[:packaging[:classifier]]` segments are ignored where the
/// operation is version-blind.
fn group_artifact(name: &str) -> Result<String> {
    let mut segments = name.split(':');
    match (segments.next(), segments.next()) {
        (Some(group), Some(artifact)) if !group.is_empty() && !artifact.is_empty() => {
            Ok(format!("{group}:{artifact}"))
        }
        _ => Err(Error::Other(format!(
            "{ID}: `{name}` is not a `group:artifact` coordinate"
        ))),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Maven"
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

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        // `dependency:get` takes one artifact per run.
        for package in packages {
            let coordinate = install_coordinate(&package.name)?;
            let cmd = self
                .cmd()
                .arg("dependency:get")
                .arg(format!("-Dartifact={coordinate}"));
            self.run(cmd, ctx).await?;
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        let coordinates = packages
            .iter()
            .map(|package| group_artifact(&package.name))
            .collect::<Result<Vec<_>>>()?
            .join(",");
        let cmd = self
            .cmd()
            .arg("dependency:purge-local-repository")
            .arg(format!("-DmanualInclude={coordinates}"))
            .args(["-DreResolve=false", "-DactTransitively=false"]);
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let repository = self.local_repository().await?;
        Ok(scan_repository(&repository)?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let coordinate = group_artifact(name)?;
        let requested = name.split(':').nth(2);
        let repository = self.local_repository().await?;
        scan_repository(&repository)?
            .into_iter()
            .filter(|package| package.name == coordinate)
            .filter(|package| requested.is_none() || package.version.as_deref() == requested)
            .max_by(|a, b| {
                compare_versions(
                    a.version.as_deref().unwrap_or(""),
                    b.version.as_deref().unwrap_or(""),
                )
            })
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }
}

/// Walk the local repository for `.pom` files - one per cached
/// artifact-version - and turn their paths into coordinates.
fn scan_repository(root: &Path) -> Result<Vec<MavenPackage>> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut poms = Vec::new();
    collect_poms(root, &mut poms)?;
    let mut packages: Vec<MavenPackage> = poms
        .iter()
        .filter_map(|pom| coordinate_from_pom(root, pom))
        .map(|(name, version)| MavenPackage {
            name,
            version: Some(version),
            state: InstallState::Installed,
        })
        .collect();
    packages.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.version.cmp(&b.version)));
    Ok(packages)
}

fn collect_poms(dir: &Path, poms: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_poms(&path, poms)?;
        } else if path.extension().is_some_and(|extension| extension == "pom") {
            poms.push(path);
        }
    }
    Ok(())
}

/// `<root>/org/apache/commons/commons-lang3/3.14.0/commons-lang3-3.14.0.pom`
/// → `("org.apache.commons:commons-lang3", "3.14.0")`: the repository
/// layout puts artifact and version in the last two directories and the
/// dotted group in everything above them.
fn coordinate_from_pom(root: &Path, pom: &Path) -> Option<(String, String)> {
    let relative = pom.strip_prefix(root).ok()?;
    let components: Vec<&str> = relative
        .iter()
        .map(|component| component.to_str())
        .collect::<Option<_>>()?;
    let [group @ .., artifact, version, _file] = components.as_slice() else {
        return None;
    };
    if group.is_empty() {
        return None;
    }
    Some((
        format!("{}:{artifact}", group.join(".")),
        (*version).to_string(),
    ))
}

/// Order two version strings by their dot/dash-separated chunks, comparing
/// numerically where both chunks are numbers ("10" beats "9") and lexically
/// otherwise - close enough to Maven's ordering to pick a newest cached
/// version.
fn compare_versions(a: &str, b: &str) -> Ordering {
    let mut left = a.split(['.', '-']);
    let mut right = b.split(['.', '-']);
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(l), Some(r)) => {
                let ordering = match (l.parse::<u64>(), r.parse::<u64>()) {
                    (Ok(l), Ok(r)) => l.cmp(&r),
                    _ => l.cmp(r),
                };
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
        }
    }
}

/// An artifact as the local repository layout describes it.
#[derive(Debug)]
pub struct MavenPackage {
    /// `group:artifact` coordinate.
    pub name: String,
    pub version: Option<String>,
    pub state: InstallState,
}

impl Package for MavenPackage {
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
    fn accepts_full_install_coordinates() {
        assert_eq!(
            install_coordinate("org.apache.commons:commons-lang3:3.14.0").unwrap(),
            "org.apache.commons:commons-lang3:3.14.0"
        );
        assert_eq!(
            install_coordinate("com.example:tool:1.0:jar:cli").unwrap(),
            "com.example:tool:1.0:jar:cli"
        );
    }

    #[test]
    fn rejects_unversioned_install_coordinates() {
        assert!(install_coordinate("org.apache.commons:commons-lang3").is_err());
        assert!(install_coordinate("commons-lang3").is_err());
        assert!(install_coordinate("group::1.0").is_err());
    }

    #[test]
    fn strips_coordinates_to_group_artifact() {
        assert_eq!(
            group_artifact("org.apache.commons:commons-lang3:3.14.0").unwrap(),
            "org.apache.commons:commons-lang3"
        );
        assert_eq!(
            group_artifact("org.apache.commons:commons-lang3").unwrap(),
            "org.apache.commons:commons-lang3"
        );
        assert!(group_artifact("commons-lang3").is_err());
    }

    #[test]
    fn rejects_at_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("org.foo:bar@1.0")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("org.foo:bar:1.0")]).is_ok());
    }

    #[test]
    fn derives_coordinates_from_pom_paths() {
        let root = Path::new("/home/nick/.m2/repository");
        let pom = root.join("org/apache/commons/commons-lang3/3.14.0/commons-lang3-3.14.0.pom");
        assert_eq!(
            coordinate_from_pom(root, &pom),
            Some((
                "org.apache.commons:commons-lang3".to_string(),
                "3.14.0".to_string()
            ))
        );
        // A pom needs at least group/artifact/version above it.
        assert_eq!(
            coordinate_from_pom(root, &root.join("junit/4.13.2/j.pom")),
            None
        );
    }

    #[test]
    fn compares_versions_numerically() {
        assert_eq!(compare_versions("1.10.0", "1.9.0"), Ordering::Greater);
        assert_eq!(compare_versions("10", "2.0"), Ordering::Greater);
        assert_eq!(compare_versions("3.14.0", "3.14.0"), Ordering::Equal);
        assert_eq!(compare_versions("1.0", "1.0-beta"), Ordering::Less);
    }
}
