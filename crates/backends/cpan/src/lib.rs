//! CPAN backend for snowcone.
//!
//! CPAN.pm command frontend plus Perl's standard installed-module inventory.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "cpan";
const PROGRAMS: &[&str] = &["cpan"];

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
    async fn installed(&self) -> Result<Vec<CpanPackage>> {
        let out = self
            .perl(LIST_SCRIPT)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_installed(&out.stdout))
    }
}

const LIST_SCRIPT: &str = r#"$i=ExtUtils::Installed->new(skip_cwd=>1);for $m(sort grep{$_ ne 'Perl'}$i->modules){$v=eval{$i->version($m)}//' ';$v=~s/[\t\r\n]/ /g;print "$m\t$v\n"}"#;
const REMOVE_SCRIPT: &str = r#"use ExtUtils::Install; $i=ExtUtils::Installed->new(skip_cwd=>1);for $m(@ARGV){$p=$i->packlist($m)->packlist_file;ExtUtils::Install::uninstall($p,1,$ENV{SNOWCONE_DRY_RUN}?1:0)}"#;
fn reject_pins(packages: &[PackageRequest]) -> Result<()> {
    if let Some(p) = packages.iter().find(|p| p.version.is_some()) {
        Err(Error::Other(format!(
            "{ID}: `{p}` cannot be pinned by the cpan client"
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
        "CPAN"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "cpan"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::SEARCH | Capabilities::UPGRADE
    }

    async fn install(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        reject_pins(_packages)?;
        self.no_dry(_ctx, "install")?;
        self.run(
            self.cmd()
                .arg("-i")
                .args(_packages.iter().map(|p| p.name.as_str())),
            _ctx,
        )
        .await
    }

    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        reject_pins(_packages)?;
        let dry = if _ctx.dry_run { "1" } else { "" };
        self.run(
            self.perl(REMOVE_SCRIPT)
                .env("SNOWCONE_DRY_RUN", dry)
                .args(_packages.iter().map(|p| p.name.as_str())),
            _ctx,
        )
        .await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.installed().await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let out = self
            .cmd()
            .arg("-D")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !out.success() {
            return Err(Error::NotFound(name.into()));
        }
        let installed = self.installed().await?.into_iter().find(|p| p.name == name);
        parse_details(&out.stdout, name, installed)
            .map(|p| Box::new(p) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.into()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let out = self
            .cmd()
            .arg("-X")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        let q = query.to_ascii_lowercase();
        Ok(boxed(
            parse_namespaces(&out.stdout)
                .into_iter()
                .filter(|p| p.name.to_ascii_lowercase().contains(&q))
                .collect(),
        ))
    }

    async fn upgrade(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        reject_pins(_packages)?;
        self.no_dry(_ctx, "upgrade")?;
        let cmd = if _packages.is_empty() {
            self.cmd().arg("-u")
        } else {
            self.cmd()
                .arg("-i")
                .args(_packages.iter().map(|p| p.name.as_str()))
        };
        self.run(cmd, _ctx).await
    }
}

fn boxed(v: Vec<CpanPackage>) -> Vec<Box<dyn Package>> {
    v.into_iter()
        .map(|p| Box::new(p) as Box<dyn Package>)
        .collect()
}
fn parse_installed(stdout: &str) -> Vec<CpanPackage> {
    stdout
        .lines()
        .filter_map(|l| {
            let (name, version) = l.split_once('\t')?;
            Some(CpanPackage {
                name: name.into(),
                version: (!version.trim().is_empty()).then(|| version.trim().into()),
                description: None,
                state: InstallState::Installed,
            })
        })
        .collect()
}
fn parse_namespaces(stdout: &str) -> Vec<CpanPackage> {
    stdout
        .lines()
        .filter_map(|l| {
            let name = l.split_whitespace().next()?;
            if !name.contains("::") && !name.chars().next().is_some_and(char::is_uppercase) {
                return None;
            }
            Some(CpanPackage {
                name: name.into(),
                version: None,
                description: None,
                state: InstallState::Available,
            })
        })
        .collect()
}
fn parse_details(
    stdout: &str,
    fallback: &str,
    installed: Option<CpanPackage>,
) -> Option<CpanPackage> {
    let mut package = installed.unwrap_or(CpanPackage {
        name: fallback.into(),
        version: None,
        description: None,
        state: InstallState::Available,
    });
    let mut found = false;
    for l in stdout.lines() {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let key = k.trim().to_ascii_lowercase();
        let value = v.trim();
        match key.as_str() {
            "module" | "name" => {
                package.name = value.into();
                found = true
            }
            "cpan version" | "cpan" if package.version.is_none() => {
                package.version = Some(value.split_whitespace().next()?.into());
                found = true
            }
            "description" => {
                package.description = Some(value.into());
                found = true
            }
            _ => {}
        }
    }
    found.then_some(package)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_inventory() {
        let p = parse_installed("DBI\t1.647\nJSON::PP\t4.16\n");
        assert_eq!(p[1].name, "JSON::PP");
    }
    #[test]
    fn filters_namespace_dump() {
        let p = parse_namespaces("DBI 1.647\nJSON::PP 4.16\nnoise line\n");
        assert_eq!(p.len(), 2);
    }
    #[test]
    fn rejects_pins() {
        assert!(reject_pins(&[PackageRequest::parse("DBI@1.647")]).is_err());
    }
}

/// The package type this backend will produce once implemented.
#[derive(Debug)]
pub struct CpanPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for CpanPackage {
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
