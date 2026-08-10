//! opam backend for snowcone.
//!
//! Drives opam's machine-oriented column output. Mutations honor opam's
//! native confirmation and dry-run switches; repository refresh is the one
//! operation for which opam offers no simulation.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "opam";
const PROGRAMS: &[&str] = &["opam"];

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
        Cmd::new(&self.program).env("OPAMCOLOR", "never")
    }

    async fn run(&self, cmd: Cmd, ctx: &OpContext) -> Result<()> {
        let output = match &ctx.events {
            Some(events) => cmd.capture(&self.elevator, Some(events)).await?,
            None => cmd.run_interactive(&self.elevator).await?,
        };
        output.require_success()?;
        Ok(())
    }

    fn mutation(&self, verb: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(verb);
        if ctx.assume_yes {
            cmd = cmd.arg("--yes");
        }
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        cmd
    }
}

fn spec(request: &PackageRequest) -> String {
    request
        .version
        .as_ref()
        .map_or_else(|| request.name.clone(), |v| format!("{}.{v}", request.name))
}

const COLUMNS: &str = "name,installed-version,version,synopsis";

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "opam"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "opam"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::REFRESH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        self.run(
            self.mutation("install", ctx)
                .args(packages.iter().map(spec)),
            ctx,
        )
        .await
    }

    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        self.run(
            self.mutation("remove", _ctx)
                .args(_packages.iter().map(|p| p.name.as_str())),
            _ctx,
        )
        .await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let out = self
            .cmd()
            .args([
                "list",
                "--installed",
                "--columns",
                COLUMNS,
                "--separator",
                "\t",
            ])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_columns(&out.stdout, InstallState::Installed)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let out = self
            .cmd()
            .args(["list", "--columns", COLUMNS, "--separator", "\t"])
            .arg(name)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        parse_columns(&out.stdout, InstallState::Available)
            .into_iter()
            .find(|p| p.name == name)
            .map(|p| Box::new(p) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.into()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let out = self
            .cmd()
            .args(["search", "--columns", COLUMNS, "--separator", "\t"])
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_columns(&out.stdout, InstallState::Available)))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: refresh has no dry-run mode")));
        }
        let mut cmd = self.cmd().arg("update");
        if ctx.assume_yes {
            cmd = cmd.arg("--yes");
        }
        self.run(cmd, ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        self.run(
            self.mutation("upgrade", ctx)
                .args(packages.iter().map(spec)),
            ctx,
        )
        .await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let out = self
            .cmd()
            .args([
                "list",
                "--outdated",
                "--columns",
                COLUMNS,
                "--separator",
                "\t",
            ])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_columns(&out.stdout, InstallState::Upgradable)))
    }
}

fn boxed(packages: Vec<OpamPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|p| Box::new(p) as Box<dyn Package>)
        .collect()
}

fn parse_columns(stdout: &str, default_state: InstallState) -> Vec<OpamPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(4, '\t');
            let name = fields.next()?.trim();
            let installed = fields.next().unwrap_or("").trim();
            let available = fields.next().unwrap_or("").trim();
            let description = fields
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            if name.is_empty() || name == "name" {
                return None;
            }
            let state = if !installed.is_empty() && !available.is_empty() && installed != available
            {
                InstallState::Upgradable
            } else if !installed.is_empty() {
                InstallState::Installed
            } else {
                default_state
            };
            let version = (!installed.is_empty())
                .then_some(installed)
                .or_else(|| (!available.is_empty()).then_some(available))
                .map(str::to_owned);
            Some(OpamPackage {
                name: name.into(),
                version,
                description,
                state,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_machine_columns_and_states() {
        let rows = parse_columns(
            "name\tinstalled-version\tversion\tsynopsis\n dune\t3.17.2\t3.18.0\tBuild system\nfmt\t\t0.11.0\tFormatting\n",
            InstallState::Available,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "dune");
        assert_eq!(rows[0].version.as_deref(), Some("3.17.2"));
        assert_eq!(rows[0].state, InstallState::Upgradable);
        assert_eq!(rows[1].state, InstallState::Available);
    }

    #[test]
    fn formats_version_constraints() {
        assert_eq!(spec(&PackageRequest::parse("dune@3.17.2")), "dune.3.17.2");
        assert_eq!(spec(&PackageRequest::parse("dune")), "dune");
    }
}

/// The package type this backend will produce once implemented.
#[derive(Debug)]
pub struct OpamPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for OpamPackage {
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
