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
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
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
        // `cpan -D` exits 0 even for unknown modules; the parse decides.
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

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let out = self
            .cmd()
            .arg("-O")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_outdated(&out.stdout)))
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
                latest_version: None,
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
                latest_version: None,
                description: None,
                state: InstallState::Available,
            })
        })
        .collect()
}
// `cpan -D`: the module name on a line of its own, a rule of dashes, then
// TAB-indented detail lines in fixed order - description (or `(no
// description)`), CPAN distribution file, install path, `Installed:
// <version|not installed>`, `CPAN:      <version>  [Not ]up to date`,
// author, email (per App::Cpan's _show_Details). The command exits 0 even
// for unknown modules, so the parse decides found/not-found: unresolved
// modules print neither the rule nor a `CPAN:` line.
fn parse_details(
    stdout: &str,
    fallback: &str,
    installed: Option<CpanPackage>,
) -> Option<CpanPackage> {
    let mut name = None;
    let mut description = None;
    let mut installed_version: Option<&str> = None;
    let mut cpan_version = None;
    let mut uptodate = false;
    let mut saw_rule = false;
    let mut saw_detail = false;
    for l in stdout.lines() {
        let indented = l.starts_with(char::is_whitespace);
        let text = l.trim();
        if text.is_empty() {
            continue;
        }
        if !indented {
            if text.len() >= 4 && text.chars().all(|c| c == '-') {
                saw_rule = true;
            } else if !saw_rule {
                // Preamble noise ("Reading .../Metadata", ...) precedes
                // the name; the line directly above the rule wins.
                name = Some(text);
            }
            continue;
        }
        if !saw_rule {
            continue;
        }
        if let Some(v) = text.strip_prefix("Installed:") {
            let v = v.trim();
            if !v.is_empty() && v != "not installed" {
                installed_version = Some(v);
            }
        } else if let Some(v) = text.strip_prefix("CPAN:") {
            cpan_version = v.split_whitespace().next();
            uptodate = !v.contains("Not up to date");
        } else if !saw_detail && text != "(no description)" {
            description = Some(text.to_string());
        }
        saw_detail = true;
    }
    if !saw_rule {
        return None;
    }
    let cpan_version = cpan_version?.to_string();
    let name = name.unwrap_or(fallback).to_string();
    let inventoried = installed.is_some();
    let installed_version = installed_version
        .map(str::to_string)
        .or(installed.and_then(|p| p.version));
    Some(if installed_version.is_some() || inventoried {
        CpanPackage {
            name,
            version: installed_version.or_else(|| Some(cpan_version.clone())),
            latest_version: (!uptodate).then_some(cpan_version),
            description,
            state: if uptodate {
                InstallState::Installed
            } else {
                InstallState::Upgradable
            },
        }
    } else {
        CpanPackage {
            name,
            version: Some(cpan_version),
            latest_version: None,
            description,
            state: InstallState::Available,
        }
    })
}
// `cpan -O`: a `Module Name  Local  CPAN` header, a rule of dashes, then
// one `name  local  cpan` row per outdated module (App::Cpan renders both
// versions via printf %.4f).
fn parse_outdated(stdout: &str) -> Vec<CpanPackage> {
    stdout
        .lines()
        .filter_map(|l| {
            let mut fields = l.split_whitespace();
            let (name, local, cpan) = (fields.next()?, fields.next()?, fields.next()?);
            if fields.next().is_some()
                || !local.starts_with(|c: char| c.is_ascii_digit())
                || !cpan.starts_with(|c: char| c.is_ascii_digit())
            {
                return None;
            }
            Some(CpanPackage {
                name: name.into(),
                version: Some(local.into()),
                latest_version: Some(cpan.into()),
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
    #[test]
    fn parses_installed_details() {
        let out = "Loading internal logger. Log::Log4perl recommended for better logging\nReading '/home/nick/.cpan/Metadata'\n  Database was generated on Mon, 21 Jul 2025 09:41:03 GMT\nText::Autoformat\n-------------------------------------------------------------------------\n\tAutomatic text wrapping and reformatting\n\tN/NE/NEILB/Text-Autoformat-1.75.tar.gz\n\t/usr/lib/perl5/site_perl/Text/Autoformat.pm\n\tInstalled: 1.74\n\tCPAN:      1.75  Not up to date\n\tNeil Bowers (NEILB)\n\tneil@bowers.uk\n\n";
        let p = parse_details(out, "Text::Autoformat", None).unwrap();
        assert_eq!(p.name, "Text::Autoformat");
        assert_eq!(p.version.as_deref(), Some("1.74"));
        assert_eq!(p.latest_version.as_deref(), Some("1.75"));
        assert_eq!(
            p.description.as_deref(),
            Some("Automatic text wrapping and reformatting")
        );
        assert_eq!(p.state, InstallState::Upgradable);
    }
    #[test]
    fn parses_uninstalled_details() {
        let out = "Bundle::CPAN\n-------------------------------------------------------------------------\n\t(no description)\n\tA/AN/ANDK/Bundle-CPAN-1.861.tar.gz\n\t(no installation file)\n\tInstalled: not installed\n\tCPAN:      1.861  Not up to date\n\tAndreas J. Koenig (ANDK)\n\tandk@cpan.org\n\n";
        let p = parse_details(out, "Bundle::CPAN", None).unwrap();
        assert_eq!(p.version.as_deref(), Some("1.861"));
        assert_eq!(p.latest_version, None);
        assert_eq!(p.description, None);
        assert_eq!(p.state, InstallState::Available);
    }
    #[test]
    fn details_keep_inventory_version() {
        let out = "JSON::PP\n-------------------------------------------------------------------------\n\t(no description)\n\tI/IS/ISHIGAKI/JSON-PP-4.16.tar.gz\n\t/usr/lib/perl5/core_perl/JSON/PP.pm\n\tInstalled: 4.16\n\tCPAN:      4.16  up to date\n\tKenichi Ishigaki (ISHIGAKI)\n\tishigaki@cpan.org\n\n";
        let inventory = CpanPackage {
            name: "JSON::PP".into(),
            version: Some("4.16".into()),
            latest_version: None,
            description: None,
            state: InstallState::Installed,
        };
        let p = parse_details(out, "JSON::PP", Some(inventory)).unwrap();
        assert_eq!(p.version.as_deref(), Some("4.16"));
        assert_eq!(p.latest_version, None);
        assert_eq!(p.state, InstallState::Installed);
    }
    #[test]
    fn unknown_module_details_parse_to_none() {
        let out = "Loading internal logger. Log::Log4perl recommended for better logging\nReading '/home/nick/.cpan/Metadata'\n  Database was generated on Mon, 21 Jul 2025 09:41:03 GMT\n";
        assert!(parse_details(out, "No::Such::Module", None).is_none());
    }
    #[test]
    fn parses_outdated_listing() {
        let out = "Module Name                                 Local    CPAN\n-------------------------------------------------------------------------\nText::Autoformat                          1.7400  1.7500\nJSON::PP                                  4.1600  4.1700\n";
        let p = parse_outdated(out);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].name, "Text::Autoformat");
        assert_eq!(p[0].version.as_deref(), Some("1.7400"));
        assert_eq!(p[0].latest_version.as_deref(), Some("1.7500"));
        assert_eq!(p[0].state, InstallState::Upgradable);
    }
}

/// The package type this backend will produce once implemented.
#[derive(Debug)]
pub struct CpanPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
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
