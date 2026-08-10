//! Julia Pkg backend for snowcone.
//!
//! Julia Pkg backend for the active environment.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "julia";
const PROGRAMS: &[&str] = &["julia"];

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
    fn expr(&self, script: &str) -> Cmd {
        Cmd::new(&self.program).args(["--startup-file=no", "--history-file=no", "-e", script, "--"])
    }

    async fn run(&self, cmd: Cmd, ctx: &OpContext) -> Result<()> {
        let output = match &ctx.events {
            Some(events) => cmd.capture(&self.elevator, Some(events)).await?,
            None => cmd.run_interactive(&self.elevator).await?,
        };
        output.require_success()?;
        Ok(())
    }

    fn no_dry_run(&self, ctx: &OpContext, operation: &str) -> Result<()> {
        if ctx.dry_run {
            Err(Error::Other(format!(
                "{ID}: {operation} has no dry-run mode"
            )))
        } else {
            Ok(())
        }
    }

    async fn installed(&self) -> Result<Vec<JuliaPackage>> {
        let output = self
            .expr(LIST_SCRIPT)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_list(&output.stdout))
    }
}

const SPEC_SCRIPT: &str = r#"using Pkg; spec(s)=begin p=split(s,'@';limit=2); length(p)==2 ? PackageSpec(name=p[1],version=p[2]) : PackageSpec(name=s) end"#;
const LIST_SCRIPT: &str = r#"using Pkg; for (_,p) in sort!(collect(Pkg.dependencies());by=x->x[2].name); p.is_direct_dep || continue; println(replace(p.name,r"[\t\r\n]"=>" "),'\t',isnothing(p.version) ? "" : string(p.version)); end"#;
const OUTDATED_SCRIPT: &str =
    r#"using Pkg; io=IOBuffer(); Pkg.status(;outdated=true,io=io); print(String(take!(io)))"#;

fn spec(request: &PackageRequest) -> String {
    request.version.as_ref().map_or_else(
        || request.name.clone(),
        |version| format!("{}@{version}", request.name),
    )
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Julia Pkg"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "julia"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        self.no_dry_run(_ctx, "install")?;
        let script = format!("{SPEC_SCRIPT}; Pkg.add([spec(s) for s in ARGS])");
        self.run(self.expr(&script).args(_packages.iter().map(spec)), _ctx)
            .await
    }

    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        self.no_dry_run(_ctx, "remove")?;
        self.run(
            self.expr("using Pkg; Pkg.rm(ARGS)")
                .args(_packages.iter().map(|package| package.name.as_str())),
            _ctx,
        )
        .await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.installed().await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        self.installed()
            .await?
            .into_iter()
            .find(|package| package.name == name)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.into()))
    }

    async fn upgrade(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        self.no_dry_run(_ctx, "upgrade")?;
        if _packages.is_empty() {
            return self.run(self.expr("using Pkg; Pkg.update()"), _ctx).await;
        }
        let script = format!(
            "{SPEC_SCRIPT}; pinned=[spec(s) for s in ARGS if occursin('@',s)]; names=[s for s in ARGS if !occursin('@',s)]; !isempty(pinned) && Pkg.add(pinned); !isempty(names) && Pkg.update(names)"
        );
        self.run(self.expr(&script).args(_packages.iter().map(spec)), _ctx)
            .await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .expr(OUTDATED_SCRIPT)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_outdated(&output.stdout)))
    }
}

fn boxed(packages: Vec<JuliaPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

fn parse_list(stdout: &str) -> Vec<JuliaPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let (name, version) = line.split_once('\t')?;
            (!name.is_empty()).then(|| JuliaPackage {
                name: name.to_owned(),
                version: (!version.is_empty()).then(|| version.to_owned()),
                description: None,
                state: InstallState::Installed,
            })
        })
        .collect()
}

fn parse_outdated(stdout: &str) -> Vec<JuliaPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if !line.starts_with(['⌃', '⌅']) {
                return None;
            }
            let after_uuid = line.split_once(']')?.1.trim();
            let mut fields = after_uuid.split_whitespace();
            let name = fields.next()?;
            let current = fields.next()?.trim_start_matches('v');
            Some(JuliaPackage {
                name: name.to_owned(),
                version: (!current.is_empty()).then(|| current.to_owned()),
                description: None,
                state: InstallState::Upgradable,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_package_specs() {
        assert_eq!(spec(&PackageRequest::parse("JSON@0.21.4")), "JSON@0.21.4");
        assert_eq!(spec(&PackageRequest::parse("JSON")), "JSON");
    }

    #[test]
    fn parses_direct_dependencies() {
        let packages = parse_list("Example\t0.5.5\nTest\t\n");
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].version.as_deref(), Some("0.5.5"));
        assert_eq!(packages[1].version, None);
    }

    #[test]
    fn parses_outdated_status() {
        let packages = parse_outdated(
            "Status `Manifest.toml`\n⌃ [a8cc5b0e] Crayons v2.0.0 [<v3.0.0]\n⌅ [b8a86587] NearestNeighbors v0.4.8 [compat]\n",
        );
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "Crayons");
        assert_eq!(packages[0].version.as_deref(), Some("2.0.0"));
    }
}

/// The package type this backend will produce once implemented.
#[derive(Debug)]
pub struct JuliaPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for JuliaPackage {
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
