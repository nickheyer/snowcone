//! Fortran fpm backend for snowcone.
//!
//! fpm dependencies are project-scoped and declared in `fpm.toml`.
//! `fpm update --fetch-only` installs declared dependencies into the build
//! dependency tree; `fpm update` upgrades them. Resolved state is read from
//! fpm's own `build/cache.toml` (or `$FPM_BUILD_DIR/cache.toml`). fpm has no
//! command that removes a dependency declaration, so removal is rejected
//! instead of editing user manifests behind fpm's back.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};
use toml::Value;

const ID: &str = "fpm";
const PROGRAMS: &[&str] = &["fpm"];

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
        let program = find_program(PROGRAMS[0]).ok_or_else(|| Error::Unavailable(ID.into()))?;
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
    fn cmd(&self) -> Cmd {
        Cmd::new(&self.program)
    }

    async fn run(&self, cmd: Cmd, ctx: &OpContext) -> Result<()> {
        let output = match &ctx.events {
            Some(events) => cmd.capture(&self.elevator, Some(events)).await?,
            None => cmd.run_interactive(&self.elevator).await?,
        };
        output.require_success()?;
        Ok(())
    }

    fn installed(&self) -> Result<Vec<FpmPackage>> {
        let build_dir = std::env::var_os("FPM_BUILD_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("build"));
        let cache = build_dir.join("cache.toml");
        let contents = std::fs::read_to_string(&cache).map_err(|error| {
            Error::Other(format!(
                "{ID}: cannot read resolved dependency cache `{}`: {error}; run `fpm update --fetch-only` first",
                cache.display()
            ))
        })?;
        parse_cache(&contents)
    }
}

fn reject_pins(packages: &[PackageRequest]) -> Result<()> {
    if let Some(package) = packages.iter().find(|package| package.version.is_some()) {
        Err(Error::Other(format!(
            "{ID}: `{package}` cannot be pinned on the command line; set its `v` constraint in fpm.toml"
        )))
    } else {
        Ok(())
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Fortran fpm"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "fpm"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::UPGRADE
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: install has no dry-run mode")));
        }
        let cmd = self
            .cmd()
            .args(["update", "--fetch-only"])
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        let target = packages
            .first()
            .map_or("a dependency", |package| package.name.as_str());
        Err(Error::Other(format!(
            "{ID}: cannot remove `{target}` by command; remove it from fpm.toml, then run `fpm update --clean`"
        )))
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.installed()?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        self.installed()?
            .into_iter()
            .find(|package| package.name == name)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.into()))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: upgrade has no dry-run mode")));
        }
        self.run(
            self.cmd()
                .arg("update")
                .args(packages.iter().map(|package| package.name.as_str())),
            ctx,
        )
        .await
    }
}

fn parse_cache(contents: &str) -> Result<Vec<FpmPackage>> {
    let document: Value =
        toml::from_str(contents).map_err(|error: toml::de::Error| Error::Parse {
            what: format!("{ID} dependency cache"),
            detail: error.to_string(),
        })?;
    let dependencies = document
        .get("dependencies")
        .and_then(Value::as_table)
        .ok_or_else(|| Error::Parse {
            what: format!("{ID} dependency cache"),
            detail: "missing [dependencies] table".into(),
        })?;
    Ok(dependencies
        .iter()
        .filter_map(|(name, value)| {
            let table = value.as_table()?;
            let directory = table.get("proj-dir").and_then(Value::as_str);
            // The root project is serialized into the same table at `.`.
            if directory == Some(".") {
                return None;
            }
            let version = table
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_string);
            let revision = table
                .get("revision")
                .and_then(Value::as_str)
                .filter(|revision| !revision.is_empty());
            Some(FpmPackage {
                name: name.clone(),
                version: version.or_else(|| revision.map(str::to_string)),
                description: None,
                path: directory.map(PathBuf::from),
                state: InstallState::Installed,
            })
        })
        .collect())
}

fn boxed(packages: Vec<FpmPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// A dependency resolved into fpm's project build cache.
#[derive(Debug)]
pub struct FpmPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub path: Option<PathBuf>,
    pub state: InstallState,
}

impl Package for FpmPackage {
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

    fn origin(&self) -> Option<&str> {
        self.path.as_deref().and_then(Path::to_str)
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fpm_dependency_cache_and_skips_root() {
        let cache = r#"
ndep = 3
[dependencies.demo]
proj-dir = "."
version = "0.1.0"
[dependencies.stdlib]
proj-dir = "build/dependencies/stdlib"
version = "0.7.0"
[dependencies.test-drive]
proj-dir = "build/dependencies/test-drive"
revision = "abc123"
"#;
        let packages = parse_cache(cache).unwrap();
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "stdlib");
        assert_eq!(packages[0].version.as_deref(), Some("0.7.0"));
        assert_eq!(packages[1].version.as_deref(), Some("abc123"));
    }

    #[test]
    fn rejects_cli_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("stdlib@0.7.0")]).is_err());
    }
}
