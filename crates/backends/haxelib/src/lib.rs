//! haxelib backend for snowcone.
//!
//! Uses haxelib's global repository and documented list/info/search formats.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "haxelib";
const PROGRAMS: &[&str] = &["haxelib"];

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
        let out = match &ctx.events {
            Some(e) => cmd.capture(&self.elevator, Some(e)).await?,
            None => cmd.run_interactive(&self.elevator).await?,
        };
        out.require_success()?;
        Ok(())
    }
    fn mutation(&self, verb: &str, ctx: &OpContext) -> Result<Cmd> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: {verb} has no dry-run mode")));
        }
        let mut cmd = self.cmd();
        if ctx.assume_yes {
            cmd = cmd.arg("--always");
        }
        Ok(cmd.arg(verb))
    }
}

fn add_request(mut cmd: Cmd, request: &PackageRequest) -> Cmd {
    cmd = cmd.arg(&request.name);
    if let Some(version) = &request.version {
        cmd = cmd.arg(version);
    }
    cmd
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "haxelib"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "haxelib"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::UPGRADE
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        for package in _packages {
            self.run(add_request(self.mutation("install", _ctx)?, package), _ctx)
                .await?;
        }
        Ok(())
    }

    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        for package in _packages {
            self.run(add_request(self.mutation("remove", _ctx)?, package), _ctx)
                .await?;
        }
        Ok(())
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let out = self
            .cmd()
            .arg("list")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_list(&out.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let out = self
            .cmd()
            .arg("info")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !out.success() {
            return Err(Error::NotFound(name.into()));
        }
        let mut package = parse_info(&out.stdout).ok_or_else(|| Error::NotFound(name.into()))?;
        if let Some(installed) = parse_list(
            &self
                .cmd()
                .arg("list")
                .arg(name)
                .capture(&self.elevator, None)
                .await?
                .stdout,
        )
        .into_iter()
        .next()
        {
            package.version = installed.version;
            package.state = InstallState::Installed;
        }
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let out = self
            .cmd()
            .arg("search")
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_search(&out.stdout)))
    }

    async fn upgrade(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        if _packages.is_empty() {
            return self.run(self.mutation("update", _ctx)?, _ctx).await;
        }
        for package in _packages {
            self.run(add_request(self.mutation("update", _ctx)?, package), _ctx)
                .await?;
        }
        Ok(())
    }
}

fn boxed(v: Vec<HaxelibPackage>) -> Vec<Box<dyn Package>> {
    v.into_iter()
        .map(|p| Box::new(p) as Box<dyn Package>)
        .collect()
}

fn parse_list(stdout: &str) -> Vec<HaxelibPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let (name, versions) = line.split_once(':')?;
            let active = versions
                .split_whitespace()
                .find(|v| v.starts_with('[') && v.ends_with(']'))
                .map(|v| v.trim_matches(&['[', ']'][..]).replace(',', "."));
            Some(HaxelibPackage {
                name: name.trim().into(),
                version: active,
                description: None,
                state: InstallState::Installed,
            })
        })
        .collect()
}

fn parse_info(stdout: &str) -> Option<HaxelibPackage> {
    let mut name = None;
    let mut version = None;
    let mut description = None;
    for line in stdout.lines() {
        if let Some((key, value)) = line.split_once(':') {
            match key.trim().to_ascii_lowercase().as_str() {
                "name" | "project" => name = Some(value.trim().to_owned()),
                "current version" | "version" => version = Some(value.trim().to_owned()),
                "description" => description = Some(value.trim().to_owned()),
                _ => {}
            }
        }
    }
    name.map(|name| HaxelibPackage {
        name,
        version,
        description,
        state: InstallState::Available,
    })
}

fn parse_search(stdout: &str) -> Vec<HaxelibPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with("Search") {
                return None;
            }
            let (name, description) = line
                .split_once(':')
                .map_or((line, None), |(n, d)| (n, Some(d.trim().to_owned())));
            Some(HaxelibPackage {
                name: name.trim().into(),
                version: None,
                description,
                state: InstallState::Available,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn list_uses_active_version() {
        let p = parse_list("format: 3.4.0 [3.5.0]\nhxcpp: [4,3,2]\n");
        assert_eq!(p[0].version.as_deref(), Some("3.5.0"));
        assert_eq!(p[1].version.as_deref(), Some("4.3.2"));
    }
    #[test]
    fn info_fields() {
        let p = parse_info("Project: openfl\nDescription: Framework\nCurrent version: 9.4.1\n")
            .unwrap();
        assert_eq!(p.name, "openfl");
        assert_eq!(p.description.as_deref(), Some("Framework"));
    }
}

/// The package type this backend will produce once implemented.
#[derive(Debug)]
pub struct HaxelibPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for HaxelibPackage {
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
