//! Puppy Package Manager backend for snowcone.
//!
//! Puppy's package manager is a GUI; its scriptable surface is thin. The
//! `petget` script installs a local .pet file passed by path - verified in
//! woof-CE's `rootfs-skeleton/usr/local/petget/petget`, which also re-execs
//! itself under `sudo -A` when not root and may still raise GUI dialogs
//! when a display is present (there is no yes-flag and no dry run). The
//! installed set is recorded in the pipe-delimited
//! `/root/.packages/user-installed-packages` registry, which reads parse
//! directly because no query CLI exists. Removal exists upstream only as
//! `petget -<fullname>`, a boot-script hook that raises GUI dialogs
//! whenever a display is present, so REMOVE is not advertised and `remove`
//! says why instead of guessing.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "petget";
const PROGRAMS: &[&str] = &["petget"];
const REGISTRY: &str = "/root/.packages/user-installed-packages";

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
    fn cmd(&self) -> Cmd {
        Cmd::new(&self.program)
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

    /// The user-installed registry file; absent means nothing was recorded.
    fn registry(&self) -> Result<Vec<PetgetPackage>> {
        match std::fs::read_to_string(REGISTRY) {
            Ok(contents) => Ok(parse_registry(&contents)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }
}

/// petget installs exactly the .pet file it is given: nothing to pin.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but petget installs exactly the .pet file it is given"
        ))),
        None => Ok(()),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Puppy Package Manager"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "puppy"
    }

    /// Not `CORE`: REMOVE is deliberately absent, because `remove` can
    /// only error here (see the module doc) and an advertised bit must
    /// mean the operation works.
    fn capabilities(&self) -> Capabilities {
        Capabilities::INSTALL | Capabilities::LIST_INSTALLED | Capabilities::INFO
    }

    /// Install is the only operation that runs the tool, and installing
    /// needs root (petget re-execs under `sudo -A` on its own when it has
    /// to). Everything else reads the registry file or refuses before any
    /// command runs.
    fn needs_elevation(&self, operation: Operation) -> bool {
        matches!(operation, Operation::Install)
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: install has no dry-run mode")));
        }
        for package in packages {
            // Verified against woof-CE's petget script: a .pet given by
            // path installs directly (`petget /root/pkg.pet`); bare names
            // go down the script's GUI/repo paths instead, so they are
            // refused here.
            if !package.name.ends_with(".pet") {
                return Err(Error::Other(format!(
                    "{ID}: `{}` is not a .pet file - petget only installs a local .pet given by \
                     path; repository installs need the Puppy Package Manager GUI",
                    package.name
                )));
            }
            // Elevated by snowcone: petget's own fallback is `sudo -A`,
            // which fails outright without an askpass helper.
            self.run(self.cmd().arg(&package.name).elevated(true), ctx)
                .await?;
        }
        Ok(())
    }

    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        // Upstream's only removal spelling, `petget -<fullname>`, is a
        // boot-script hook that raises GUI dialogs whenever a display is
        // present - not a scriptable verb, so REMOVE is not advertised.
        Err(Error::Other(format!(
            "{ID}: Puppy exposes package removal only through the Puppy Package Manager GUI \
             (petget's `-<fullname>` form is for boot scripts and raises GUI dialogs under X)"
        )))
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .registry()?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // Only the local registry is queryable; repository metadata lives
        // behind the GUI.
        self.registry()?
            .into_iter()
            .find(|package| package.name == name)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }
}

/// One registry line per package in Woof's pipe-delimited database format:
/// `fullname|name|version|release|category|size|path|filename|deps|description|…`,
/// where deps are `+name` entries joined by commas and size looks like
/// `1836K`.
fn parse_registry(contents: &str) -> Vec<PetgetPackage> {
    contents
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('|').collect();
            let full = fields.first()?.trim();
            if full.is_empty() {
                return None;
            }
            let field = |index: usize| {
                fields
                    .get(index)
                    .map(|value| value.trim())
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            };
            Some(PetgetPackage {
                name: field(1).unwrap_or_else(|| full.to_string()),
                version: field(2),
                origin: field(4),
                download_size: fields.get(5).and_then(|size| parse_size(size)),
                dependencies: field(8).map(|deps| {
                    deps.split(',')
                        .map(|dep| dep.trim().trim_start_matches('+'))
                        .filter(|dep| !dep.is_empty())
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                }),
                description: field(9),
                state: InstallState::Installed,
            })
        })
        .collect()
}

/// Registry sizes like `1836K`; only K/M-suffixed values are trusted.
fn parse_size(field: &str) -> Option<u64> {
    let field = field.trim();
    let (digits, multiplier) = match field.strip_suffix(['K', 'k']) {
        Some(digits) => (digits, 1024),
        None => (field.strip_suffix(['M', 'm'])?, 1024 * 1024),
    };
    digits.parse::<u64>().ok().map(|kilo| kilo * multiplier)
}

/// A package as Puppy's registry describes it.
#[derive(Debug, Default)]
pub struct PetgetPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub origin: Option<String>,
    pub download_size: Option<u64>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for PetgetPackage {
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
        self.origin.as_deref()
    }

    fn download_size(&self) -> Option<u64> {
        self.download_size
    }

    fn dependencies(&self) -> Option<Vec<String>> {
        self.dependencies.clone()
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_registry_lines() {
        let contents = "\
abiword-2.8.6-w5c|abiword|2.8.6|w5c|Document|1836K|pet_packages-wary5|abiword-2.8.6-w5c.pet|+glibc,+gtk+|word processor|wary|5|official|
geany-1.24|geany|1.24||Utility|3M|pet_packages|geany-1.24.pet||lightweight IDE|||official|
";
        let packages = parse_registry(contents);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "abiword");
        assert_eq!(packages[0].version.as_deref(), Some("2.8.6"));
        assert_eq!(packages[0].origin.as_deref(), Some("Document"));
        assert_eq!(packages[0].download_size, Some(1836 * 1024));
        assert_eq!(
            packages[0].dependencies,
            Some(vec!["glibc".to_string(), "gtk+".to_string()])
        );
        assert_eq!(packages[0].description.as_deref(), Some("word processor"));
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[1].dependencies, None);
        assert_eq!(packages[1].download_size, Some(3 * 1024 * 1024));
    }

    #[test]
    fn tolerates_short_and_blank_lines() {
        let packages = parse_registry("\n|x\nbare-1.0|bare|1.0\n");
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "bare");
        assert_eq!(packages[0].version.as_deref(), Some("1.0"));
    }

    #[test]
    fn parses_sizes_conservatively() {
        assert_eq!(parse_size("1836K"), Some(1836 * 1024));
        assert_eq!(parse_size("2M"), Some(2 * 1024 * 1024));
        assert_eq!(parse_size("1836"), None);
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("abiword@2.8.6")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("abiword")]).is_ok());
    }
}
