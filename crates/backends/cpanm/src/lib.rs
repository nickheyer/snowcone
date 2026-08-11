//! cpanminus backend for snowcone.
//!
//! cpanminus is deliberately only an installer. Installed-module discovery
//! and removal therefore use Perl's standard `ExtUtils::Installed` and
//! `ExtUtils::Install` APIs against the same active Perl installation.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "cpanm";
const PROGRAMS: &[&str] = &["cpanm"];

const LIST_SCRIPT: &str = r#"$i=ExtUtils::Installed->new(skip_cwd=>1);for $m(sort grep{$_ ne 'Perl'}$i->modules){$v=eval{$i->version($m)}//'';$v=~s/[\t\r\n]/ /g;print "$m\t$v\n"}"#;
const REMOVE_SCRIPT: &str = r#"use ExtUtils::Install; $i=ExtUtils::Installed->new(skip_cwd=>1);for $m(@ARGV){$p=$i->packlist($m)->packlist_file;ExtUtils::Install::uninstall($p,1,$ENV{SNOWCONE_DRY_RUN}?1:0)}"#;

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
        let perl = find_program("perl")
            .ok_or_else(|| Error::Unavailable(format!("{ID}: `perl` not found on PATH")))?;
        Ok(Box::new(Manager {
            program,
            perl,
            elevator: Elevator::detect(host),
        }))
    }
}

struct Manager {
    program: PathBuf,
    perl: PathBuf,
    elevator: Elevator,
}

impl Manager {
    fn cmd(&self) -> Cmd {
        Cmd::new(&self.program)
            .env("PERL_MM_USE_DEFAULT", "1")
            .env("NONINTERACTIVE_TESTING", "1")
            .env("LC_ALL", "C")
    }

    fn perl(&self, script: &str) -> Cmd {
        Cmd::new(&self.perl).args(["-MExtUtils::Installed", "-e", script, "--"])
    }

    async fn run(&self, cmd: Cmd, ctx: &OpContext) -> Result<()> {
        let output = match &ctx.events {
            Some(events) => cmd.capture(&self.elevator, Some(events)).await?,
            None => cmd.run_interactive(&self.elevator).await?,
        };
        output.require_success()?;
        Ok(())
    }

    async fn installed(&self) -> Result<Vec<CpanmPackage>> {
        let output = self
            .perl(LIST_SCRIPT)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_installed(&output.stdout))
    }
}

fn spec(package: &PackageRequest) -> String {
    match &package.version {
        Some(version) => format!("{}@{version}", package.name),
        None => package.name.clone(),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "cpanminus"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "cpan"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: install has no dry-run mode")));
        }
        self.run(self.cmd().args(packages.iter().map(spec)), ctx)
            .await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let dry_run = if ctx.dry_run { "1" } else { "" };
        let cmd = self
            .perl(REMOVE_SCRIPT)
            .env("SNOWCONE_DRY_RUN", dry_run)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
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
}

fn parse_installed(stdout: &str) -> Vec<CpanmPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let (name, version) = line.split_once('\t')?;
            Some(CpanmPackage {
                name: name.into(),
                version: (!version.trim().is_empty()).then(|| version.trim().into()),
                description: None,
                state: InstallState::Installed,
            })
        })
        .collect()
}

fn boxed(packages: Vec<CpanmPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// A Perl module installed in the active Perl library tree.
#[derive(Debug)]
pub struct CpanmPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for CpanmPackage {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_cpanminus_specs() {
        assert_eq!(spec(&PackageRequest::parse("DBI@1.647")), "DBI@1.647");
        assert_eq!(spec(&PackageRequest::parse("JSON::PP")), "JSON::PP");
    }

    #[test]
    fn parses_perl_inventory() {
        let packages = parse_installed("DBI\t1.647\nJSON::PP\t4.16\nNoVersion\t\n");
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[1].name, "JSON::PP");
        assert_eq!(packages[1].version.as_deref(), Some("4.16"));
        assert_eq!(packages[2].version, None);
    }
}
