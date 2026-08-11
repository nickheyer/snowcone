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

/// `available-versions` rather than `version`: opam's `version` column
/// prints the version of the row's selected package, which for an
/// installed package is the installed version - useless for spotting
/// updates. The available list is ascending, so its last entry is the
/// newest installable version.
const COLUMNS: &str = "name,installed-version,available-versions,synopsis";

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

    /// opam list has no `--outdated` selector; outdated packages are the
    /// installed ones whose newest available version differs from the
    /// installed version.
    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
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
        Ok(boxed(
            parse_columns(&out.stdout, InstallState::Installed)
                .into_iter()
                .filter(|package| package.state == InstallState::Upgradable)
                .collect(),
        ))
    }
}

fn boxed(packages: Vec<OpamPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|p| Box::new(p) as Box<dyn Package>)
        .collect()
}

/// Tab-separated `opam list --columns` rows. Header rows all start with
/// `#`: the `# Packages matching: …` banner and the `# Name\t# Installed\t…`
/// column-title row. The available-versions cell lists every version in
/// ascending order, so its last entry is the newest.
fn parse_columns(stdout: &str, default_state: InstallState) -> Vec<OpamPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            if line.trim_start().starts_with('#') {
                return None;
            }
            let mut fields = line.splitn(4, '\t');
            let name = fields.next()?.trim();
            let installed = fields.next().unwrap_or("").trim();
            let latest = fields
                .next()
                .unwrap_or("")
                .split_whitespace()
                .next_back()
                .unwrap_or("");
            let description = fields
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            if name.is_empty() {
                return None;
            }
            let state = if !installed.is_empty() && !latest.is_empty() && installed != latest {
                InstallState::Upgradable
            } else if !installed.is_empty() {
                InstallState::Installed
            } else {
                default_state
            };
            let version = (!installed.is_empty())
                .then_some(installed)
                .or_else(|| (!latest.is_empty()).then_some(latest))
                .map(str::to_owned);
            Some(OpamPackage {
                name: name.into(),
                version,
                latest_version: (!latest.is_empty()).then(|| latest.to_owned()),
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
        // Header shape as opam prints it (a `# Packages matching:` banner
        // plus a `# `-prefixed title per column, cf. opam's reftests);
        // cells are tab-separated and the available-versions cell is an
        // ascending space-joined list.
        let rows = parse_columns(
            "# Packages matching: installed\n\
             # Name\t# Installed\t# Available versions\t# Synopsis\n\
             dune\t3.17.2\t3.16.1  3.17.2  3.18.0\tFast, portable, and opinionated build system\n\
             fmt\t\t0.9.0  0.11.0\tOCaml Format pretty-printer combinators\n\
             ocamlfind\t1.9.6\t1.9.5  1.9.6\tA library manager for OCaml\n",
            InstallState::Available,
        );
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].name, "dune");
        assert_eq!(rows[0].version.as_deref(), Some("3.17.2"));
        assert_eq!(rows[0].latest_version.as_deref(), Some("3.18.0"));
        assert_eq!(rows[0].state, InstallState::Upgradable);
        assert_eq!(rows[1].state, InstallState::Available);
        assert_eq!(rows[1].version.as_deref(), Some("0.11.0"));
        assert_eq!(rows[2].state, InstallState::Installed);
    }

    #[test]
    fn formats_version_constraints() {
        assert_eq!(spec(&PackageRequest::parse("dune@3.17.2")), "dune.3.17.2");
        assert_eq!(spec(&PackageRequest::parse("dune")), "dune");
    }
}

/// A package as opam describes it.
#[derive(Debug)]
pub struct OpamPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
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
