//! EasyBuild backend for snowcone.
//!
//! EasyBuild (`eb`) is a build tool, not a database: software is described
//! by easyconfig names (`zlib-1.2.13.eb`), so install specs get `.eb`
//! appended and run through `eb <spec> --robot` to resolve dependencies -
//! easyconfigs first, `--robot` last, because `--robot` swallows a
//! following path argument (easybuild-framework #2086). There is no
//! uninstall verb at all - removal means deleting the install directory
//! and its module file by hand - so REMOVE is not advertised.
//! Installed software really lives in the module system; the defensible
//! subset here is `eb --list-installed-software`, which yields names but no
//! versions. `--dry-run` is native to `eb` itself. Builds land in the
//! user's prefix - nothing elevates.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "easybuild";
const PROGRAMS: &[&str] = &["eb"];

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
        let program = PROGRAMS
            .iter()
            .find_map(|program| find_program(program))
            .ok_or_else(|| Error::Unavailable(ID.to_string()))?;
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
    /// Mutating invocation, in the user's locale (output is passed through).
    fn cmd(&self) -> Cmd {
        Cmd::new(&self.program)
    }

    /// Read invocation with a stable locale, so parsing survives i18n.
    fn query(&self) -> Cmd {
        Cmd::new(&self.program).env("LC_ALL", "C")
    }

    /// CLI passthrough when no event consumer is attached, captured and
    /// streamed otherwise.
    async fn run(&self, cmd: Cmd, ctx: &OpContext) -> Result<()> {
        let output = match &ctx.events {
            Some(events) => cmd.capture(&self.elevator, Some(events)).await?,
            None => cmd.run_interactive(&self.elevator).await?,
        };
        output.require_success()?;
        Ok(())
    }
}

/// Version selection happens through the easyconfig name, not a separate
/// pin, so versioned requests are refused with a pointer at the native
/// spelling.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests
        .iter()
        .find_map(|request| request.version.as_deref().map(|version| (request, version)))
    {
        Some((pinned, version)) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but eb installs by easyconfig name \
             (try `{}-{version}`)",
            pinned.name
        ))),
        None => Ok(()),
    }
}

/// `name` → `name.eb` unless the request already names an easyconfig file.
fn easyconfig(name: &str) -> String {
    if name.ends_with(".eb") {
        name.to_string()
    } else {
        format!("{name}.eb")
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "EasyBuild"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Universal
    }

    fn database_id(&self) -> &'static str {
        "easybuild"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::INSTALL
            | Capabilities::LIST_INSTALLED
            | Capabilities::INFO
            | Capabilities::SEARCH
    }

    /// Easyconfigs go first: `--robot` takes an optional robot-path
    /// argument and would swallow a following `<spec>.eb`
    /// (easybuild-framework #2086), so the flags trail the targets.
    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        let mut cmd = self
            .cmd()
            .args(packages.iter().map(|package| easyconfig(&package.name)))
            .arg("--robot");
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        self.run(cmd, ctx).await
    }

    /// EasyBuild has no uninstall verb; removal is deleting the install
    /// directory and its module file by hand.
    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        Err(self.unsupported(Operation::Remove))
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("--list-installed-software")
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_installed(&output.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query()
            .arg("--search")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        // The last exact-name hit is the newest-sorted easyconfig.
        let mut package = parse_search(&output.stdout)
            .into_iter()
            .rfind(|package| package.name == name)
            .ok_or_else(|| Error::NotFound(name.to_string()))?;
        // `--search` only proves an easyconfig exists; the installed list
        // says whether a module was actually built from it.
        let installed = self
            .query()
            .arg("--list-installed-software")
            .capture(&self.elevator, None)
            .await?;
        if installed.success()
            && parse_installed(&installed.stdout)
                .iter()
                .any(|installed| installed.name == package.name)
        {
            package.state = InstallState::Installed;
        }
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("--search")
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_search(&output.stdout)))
    }
}

fn boxed(packages: Vec<EasybuildPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `eb --search`: `* <path>.eb` hit lines, sometimes shortened through a
/// `CFGS<n>=<prefix>` variable (`* $CFGS1/z/zlib/zlib-1.2.13.eb`); `==`
/// status lines and the CFGS definition itself carry no hits.
fn parse_search(stdout: &str) -> Vec<EasybuildPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let path = line.trim().strip_prefix("* ")?.trim();
            let file = path.rsplit('/').next()?.strip_suffix(".eb")?;
            let (name, version) = split_easyconfig(file);
            Some(EasybuildPackage {
                name,
                version,
                state: InstallState::Available,
            })
        })
        .collect()
}

/// `name-version[-toolchain…]` → the software name and the full version
/// suffix (toolchain included), split at the first dash followed by a
/// digit, easyconfig-style.
fn split_easyconfig(file: &str) -> (String, Option<String>) {
    for (idx, _) in file.match_indices('-') {
        if file[idx + 1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
        {
            return (file[..idx].to_string(), Some(file[idx + 1..].to_string()));
        }
    }
    (file.to_string(), None)
}

/// `eb --list-installed-software`: `* <name>` bullet lines below `==`
/// status noise; versions live in the module system, not here.
fn parse_installed(stdout: &str) -> Vec<EasybuildPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let name = line.trim().strip_prefix("* ")?.trim();
            (!name.is_empty()).then(|| EasybuildPackage {
                name: name.to_string(),
                version: None,
                state: InstallState::Installed,
            })
        })
        .collect()
}

/// A package as EasyBuild describes it.
#[derive(Debug)]
pub struct EasybuildPackage {
    pub name: String,
    pub version: Option<String>,
    pub state: InstallState,
}

impl Package for EasybuildPackage {
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
    fn parses_search_hits() {
        let stdout = "\
== found valid index for /home/nick/.local/easybuild/easyconfigs, so using it...
 * /home/nick/.local/easybuild/easyconfigs/z/zlib/zlib-1.2.13.eb
 * /home/nick/.local/easybuild/easyconfigs/z/zlib/zlib-1.3.1-GCCcore-13.3.0.eb
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "zlib");
        assert_eq!(packages[0].version.as_deref(), Some("1.2.13"));
        assert_eq!(packages[1].version.as_deref(), Some("1.3.1-GCCcore-13.3.0"));
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn parses_search_hits_with_cfgs_prefix() {
        let stdout = "\
== found valid index for /opt/easybuild/easyconfigs, so using it...
CFGS1=/opt/easybuild/easyconfigs
 * $CFGS1/b/Bison/Bison-3.8.2.eb
 * $CFGS1/b/Bison/Bison-3.8.2-GCCcore-13.2.0.eb
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "Bison");
        assert_eq!(packages[0].version.as_deref(), Some("3.8.2"));
        assert_eq!(packages[1].version.as_deref(), Some("3.8.2-GCCcore-13.2.0"));
    }

    #[test]
    fn splits_easyconfig_names() {
        assert_eq!(
            split_easyconfig("GCC-13.2.0"),
            ("GCC".to_string(), Some("13.2.0".to_string()))
        );
        assert_eq!(
            split_easyconfig("netCDF-Fortran-4.6.1-gompi-2023b"),
            (
                "netCDF-Fortran".to_string(),
                Some("4.6.1-gompi-2023b".to_string())
            )
        );
        assert_eq!(
            split_easyconfig("EasyBuild"),
            ("EasyBuild".to_string(), None)
        );
    }

    #[test]
    fn parses_installed_software() {
        let stdout = "\
== found valid index for /opt/easybuild/easyconfigs, so using it...
* EasyBuild
* GCC
* zlib
";
        let packages = parse_installed(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[1].name, "GCC");
        assert_eq!(packages[1].version, None);
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn formats_easyconfig_specs() {
        assert_eq!(easyconfig("zlib-1.2.13"), "zlib-1.2.13.eb");
        assert_eq!(easyconfig("zlib-1.2.13.eb"), "zlib-1.2.13.eb");
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("zlib@1.2.13")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("zlib-1.2.13")]).is_ok());
    }
}
