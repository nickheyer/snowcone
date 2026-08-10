//! Ant + Ivy backend for snowcone.
//!
//! Ivy has no first-class CLI here: detection keys off `ant`, and Ivy's
//! retrieval runs as Ant tasks inside a project build file, which leaves
//! bare `ant` nothing to drive - install fails with an explanation rather
//! than fake anything. The package database that does exist is Ivy's
//! resolution cache (`~/.ivy2/cache`, one `<org>/<module>` directory per
//! module with an `ivy-<rev>.xml` descriptor per cached revision), and the
//! reads work directly against that layout; remove deletes a module's
//! cache directory, the accepted way to evict Ivy cache entries. Names
//! follow the same `org:module` coordinate contract as the Maven backend.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Detection, Error, HostInfo, InstallState, ManagerKind, OpContext,
    Package, PackageManager, PackageRequest, ProgressEvent, Result, find_program,
};

const ID: &str = "ivy";
const PROGRAMS: &[&str] = &["ant"];

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

    fn create(&self, _host: &HostInfo) -> Result<Box<dyn PackageManager>> {
        Ok(Box::new(Manager))
    }
}

struct Manager;

/// Ivy's default resolution cache; a custom ivysettings relocation is not
/// visible from outside a build, so the default is all there is to honor.
fn cache_dir() -> Result<PathBuf> {
    let home =
        std::env::var_os("HOME").ok_or_else(|| Error::Other(format!("{ID}: HOME is not set")))?;
    Ok(PathBuf::from(home).join(".ivy2").join("cache"))
}

/// The `org:module` parts of a coordinate; a trailing `:revision` is
/// accepted and ignored where the operation is revision-blind.
fn split_coordinate(name: &str) -> Result<(&str, &str)> {
    let mut segments = name.split(':');
    match (segments.next(), segments.next()) {
        (Some(org), Some(module)) if !org.is_empty() && !module.is_empty() => Ok((org, module)),
        _ => Err(Error::Other(format!(
            "{ID}: `{name}` is not an `org:module` coordinate"
        ))),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Ant + Ivy"
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
        Err(Error::Other(format!(
            "{ID}: ivy retrieval runs through Ivy's Ant tasks inside a project build \
             file (or standalone `java -jar ivy.jar`); bare `ant` has no verb for it"
        )))
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cache = cache_dir()?;
        for package in packages {
            let (org, module) = split_coordinate(&package.name)?;
            let path = cache.join(org).join(module);
            if !path.is_dir() {
                return Err(Error::NotFound(package.name.clone()));
            }
            let message = if ctx.dry_run {
                format!("would remove {}", path.display())
            } else {
                std::fs::remove_dir_all(&path)?;
                format!("removed {}", path.display())
            };
            match &ctx.events {
                Some(_) => ctx.emit(ProgressEvent::Status(message)),
                None => println!("{message}"),
            }
        }
        Ok(())
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(scan_cache(&cache_dir()?)?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let (org, module) = split_coordinate(name)?;
        let coordinate = format!("{org}:{module}");
        let requested = name.split(':').nth(2);
        scan_cache(&cache_dir()?)?
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

/// Walk `<cache>/<org>/<module>` directories, reading cached revisions from
/// the `ivy-<rev>.xml` descriptor files each module directory holds.
fn scan_cache(cache: &Path) -> Result<Vec<IvyPackage>> {
    if !cache.is_dir() {
        return Ok(Vec::new());
    }
    let mut packages = Vec::new();
    for (org, org_path) in subdirectories(cache)? {
        for (module, module_path) in subdirectories(&org_path)? {
            for entry in std::fs::read_dir(&module_path)? {
                let file_name = entry?.file_name();
                let Some(file_name) = file_name.to_str() else {
                    continue;
                };
                if let Some(revision) = revision_from_descriptor(file_name) {
                    packages.push(IvyPackage {
                        name: format!("{org}:{module}"),
                        version: Some(revision.to_string()),
                        state: InstallState::Installed,
                    });
                }
            }
        }
    }
    packages.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.version.cmp(&b.version)));
    Ok(packages)
}

/// Directory entries of `dir` that are themselves directories, as
/// (name, path) pairs.
fn subdirectories(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut directories = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        directories.push((name, path));
    }
    Ok(directories)
}

/// `ivy-2.17.2.xml` → `2.17.2`; anything else in a module directory
/// (`jars/`, `ivydata-*.properties`, `ivy-*.xml.original`, …) is not a
/// cached descriptor.
fn revision_from_descriptor(file_name: &str) -> Option<&str> {
    file_name.strip_prefix("ivy-")?.strip_suffix(".xml")
}

/// Order two revision strings by their dot/dash-separated chunks, comparing
/// numerically where both chunks are numbers ("10" beats "9") and lexically
/// otherwise - enough to pick a newest cached revision.
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

/// A module as the Ivy cache layout describes it.
#[derive(Debug)]
pub struct IvyPackage {
    /// `org:module` coordinate.
    pub name: String,
    pub version: Option<String>,
    pub state: InstallState,
}

impl Package for IvyPackage {
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
    fn reads_revisions_from_descriptor_names() {
        assert_eq!(revision_from_descriptor("ivy-2.17.2.xml"), Some("2.17.2"));
        assert_eq!(
            revision_from_descriptor("ivy-1.0-beta1.xml"),
            Some("1.0-beta1")
        );
        assert_eq!(revision_from_descriptor("ivy-2.17.2.xml.original"), None);
        assert_eq!(revision_from_descriptor("ivydata-2.17.2.properties"), None);
        assert_eq!(revision_from_descriptor("commons-lang3-2.17.2.jar"), None);
    }

    #[test]
    fn splits_coordinates() {
        assert_eq!(
            split_coordinate("org.apache.ant:ant").unwrap(),
            ("org.apache.ant", "ant")
        );
        // A trailing revision is accepted and ignored.
        assert_eq!(
            split_coordinate("org.apache.ant:ant:1.10.14").unwrap(),
            ("org.apache.ant", "ant")
        );
        assert!(split_coordinate("ant").is_err());
        assert!(split_coordinate(":ant").is_err());
    }

    #[test]
    fn compares_revisions_numerically() {
        assert_eq!(compare_versions("1.10.0", "1.9.0"), Ordering::Greater);
        assert_eq!(compare_versions("2.17.2", "2.17.2"), Ordering::Equal);
        assert_eq!(compare_versions("1.0", "1.0-beta"), Ordering::Less);
    }
}
