//! R packages backend for snowcone.
//!
//! Uses non-interactive R expressions with package names passed as arguments.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "r";
const PROGRAMS: &[&str] = &["Rscript"];

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
        // Rscript already routes everything after `-e <expr>` to
        // commandArgs(trailingOnly=TRUE); an explicit `--args` literal here
        // would itself land in those arguments.
        Cmd::new(&self.program).args(["--vanilla", "-e", script])
    }
    async fn run(&self, cmd: Cmd, ctx: &OpContext) -> Result<()> {
        let out = match &ctx.events {
            Some(e) => cmd.capture(&self.elevator, Some(e)).await?,
            None => cmd.run_interactive(&self.elevator).await?,
        };
        out.require_success()?;
        Ok(())
    }
    fn no_dry(&self, ctx: &OpContext, op: &str) -> Result<()> {
        if ctx.dry_run {
            Err(Error::Other(format!("{ID}: {op} has no dry-run mode")))
        } else {
            Ok(())
        }
    }
}

// Title is not a default column of either matrix; `fields="Title"` adds it
// (from DESCRIPTION for installed.packages, from the repository PACKAGES
// index for available.packages, where a repository that omits it yields NA
// and the empty-string na= drops the description downstream).
const INSTALLED: &str = r#"x<-as.data.frame(installed.packages(fields="Title")[,c("Package","Version","Title"),drop=FALSE]);x$Title<-gsub("[\\t\\r\\n]+"," ",x$Title);write.table(x,sep="\t",row.names=FALSE,col.names=FALSE,quote=FALSE,na="")"#;
const AVAILABLE: &str = r#"a<-as.data.frame(available.packages(fields="Title")[,c("Package","Version","Title"),drop=FALSE]);n<-commandArgs(trailingOnly=TRUE);a<-a[a$Package%in%n,,drop=FALSE];a$Title<-gsub("[\\t\\r\\n]+"," ",a$Title);write.table(a,sep="\t",row.names=FALSE,col.names=FALSE,quote=FALSE,na="")"#;
const OUTDATED: &str = r#"x<-old.packages();if(!is.null(x)){x<-as.data.frame(x[,c("Package","Installed","ReposVer"),drop=FALSE]);write.table(x,sep="\t",row.names=FALSE,col.names=FALSE,quote=FALSE,na="")}"#;

fn reject_pins(packages: &[PackageRequest]) -> Result<()> {
    if let Some(p) = packages.iter().find(|p| p.version.is_some()) {
        Err(Error::Other(format!(
            "{ID}: `{p}` cannot be pinned by base R install.packages"
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
        "R packages"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "r-library"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::UPGRADE | Capabilities::LIST_OUTDATED
    }

    async fn install(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        reject_pins(_packages)?;
        self.no_dry(_ctx, "install")?;
        self.run(
            self.expr("install.packages(commandArgs(trailingOnly=TRUE),dependencies=TRUE)")
                .args(_packages.iter().map(|p| p.name.as_str())),
            _ctx,
        )
        .await
    }

    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        reject_pins(_packages)?;
        self.no_dry(_ctx, "remove")?;
        self.run(
            self.expr("remove.packages(commandArgs(trailingOnly=TRUE))")
                .args(_packages.iter().map(|p| p.name.as_str())),
            _ctx,
        )
        .await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let out = self
            .expr(INSTALLED)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_rows(&out.stdout, InstallState::Installed)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let installed = self
            .expr(INSTALLED)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        if let Some(p) = parse_rows(&installed.stdout, InstallState::Installed)
            .into_iter()
            .find(|p| p.name == name)
        {
            return Ok(Box::new(p));
        }
        let out = self
            .expr(AVAILABLE)
            .arg(name)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        parse_rows(&out.stdout, InstallState::Available)
            .into_iter()
            .next()
            .map(|p| Box::new(p) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.into()))
    }

    async fn upgrade(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        reject_pins(_packages)?;
        self.no_dry(_ctx, "upgrade")?;
        let cmd = if _packages.is_empty() {
            self.expr("update.packages(ask=FALSE,checkBuilt=TRUE)")
        } else {
            self.expr("install.packages(commandArgs(trailingOnly=TRUE),dependencies=TRUE)")
                .args(_packages.iter().map(|p| p.name.as_str()))
        };
        self.run(cmd, _ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let out = self
            .expr(OUTDATED)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_outdated(&out.stdout)))
    }
}

fn boxed(v: Vec<RPackage>) -> Vec<Box<dyn Package>> {
    v.into_iter()
        .map(|p| Box::new(p) as Box<dyn Package>)
        .collect()
}
fn parse_rows(stdout: &str, state: InstallState) -> Vec<RPackage> {
    stdout
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            let name = f.next()?.trim();
            if name.is_empty() {
                return None;
            }
            Some(RPackage {
                name: name.into(),
                version: f.next().map(str::to_owned),
                description: f.next().filter(|s| !s.is_empty()).map(str::to_owned),
                state,
            })
        })
        .collect()
}
fn parse_outdated(stdout: &str) -> Vec<RPackage> {
    stdout
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let name = f.next()?;
            let _installed = f.next()?;
            let available = f.next()?;
            Some(RPackage {
                name: name.into(),
                version: Some(available.into()),
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
    fn parses_installed_rows() {
        let p = parse_rows(
            "dplyr\t1.1.4\tA Grammar of Data Manipulation\n",
            InstallState::Installed,
        );
        assert_eq!(
            p[0].description.as_deref(),
            Some("A Grammar of Data Manipulation")
        );
    }
    #[test]
    fn parses_outdated_rows() {
        let p = parse_outdated("dplyr\t1.1.3\t1.1.4\n");
        assert_eq!(p[0].version.as_deref(), Some("1.1.4"));
    }
    #[test]
    fn rejects_pins() {
        assert!(reject_pins(&[PackageRequest::parse("dplyr@1.1.4")]).is_err());
    }
}

/// The package type this backend will produce once implemented.
#[derive(Debug)]
pub struct RPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for RPackage {
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
