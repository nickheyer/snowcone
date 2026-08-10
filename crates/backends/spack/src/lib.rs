//! Spack backend for snowcone.
//!
//! Spack is a from-source HPC manager whose installs are hash-addressed:
//! nothing upgrades in place, so `upgrade` reinstalls the named packages at
//! their latest versions with `--fresh` (the default reuse concretizer
//! would happily resolve straight back to the already-installed hash) and
//! an upgrade-all pass does not exist. Installed state comes from `spack
//! find --json`; the available universe is `spack list` (names only, one
//! per line when piped) and `spack info`, both line-parsed under
//! `LC_ALL=C`. No spack verb has a dry-run switch, so `--dry-run` requests
//! error out. Installs are per-tree and user-owned - nothing elevates.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "spack";
const PROGRAMS: &[&str] = &["spack"];

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

    fn no_dry_run(&self, operation: &str) -> Error {
        Error::Other(format!("{ID}: {operation} has no dry-run mode"))
    }
}

/// Spack specs pin natively (`name@version`), but this backend does not
/// advertise pinning, so versioned requests are refused rather than half
/// honored.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but this backend does not support version pins"
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
        "Spack"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Universal
    }

    fn database_id(&self) -> &'static str {
        "spack"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::SEARCH | Capabilities::UPGRADE
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        let cmd = self
            .cmd()
            .arg("install")
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("uninstall"));
        }
        let mut cmd = self.cmd().arg("uninstall");
        if ctx.assume_yes {
            cmd = cmd.arg("-y");
        }
        cmd = cmd.args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .cmd()
            .args(["find", "--json"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        let json: Value = serde_json::from_str(&output.stdout).map_err(|error| Error::Parse {
            what: format!("{ID} find output"),
            detail: error.to_string(),
        })?;
        Ok(boxed(parse_find(&json)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query()
            .arg("info")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        let mut package = parse_info(&output.stdout).ok_or_else(|| Error::Parse {
            what: format!("{ID} info output"),
            detail: format!("unrecognized layout for `{name}`"),
        })?;
        // `spack info` describes the recipe; the install database says
        // whether (and at which version) it is actually installed.
        let find = self
            .cmd()
            .args(["find", "--json"])
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if find.success()
            && let Ok(json) = serde_json::from_str::<Value>(&find.stdout)
            && let Some(installed) = parse_find(&json)
                .into_iter()
                .find(|installed| installed.name == package.name)
        {
            package.state = InstallState::Installed;
            if installed.version.is_some() && installed.version != package.version {
                package.latest_version = package.version.take();
                package.version = installed.version;
            }
            if package.architecture.is_none() {
                package.architecture = installed.architecture;
            }
        }
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("list")
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_list(&output.stdout)))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if packages.is_empty() {
            return Err(Error::Other(format!(
                "{ID}: installs are hash-addressed and nothing tracks a whole-tree upgrade; \
                 name the packages to reinstall at their latest versions"
            )));
        }
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        let cmd = self
            .cmd()
            .args(["install", "--fresh"])
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }
}

fn boxed(packages: Vec<SpackPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `spack find --json`: an array of concrete spec records (`name`,
/// `version`, `arch`, `namespace`, …); `arch.target` is a plain string on
/// older spack and an object carrying a `name` on newer.
fn parse_find(json: &Value) -> Vec<SpackPackage> {
    let Some(entries) = json.as_array() else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let target = &entry["arch"]["target"];
            Some(SpackPackage {
                name: entry["name"].as_str()?.to_string(),
                version: entry["version"].as_str().map(str::to_string),
                architecture: target
                    .as_str()
                    .or_else(|| target["name"].as_str())
                    .map(str::to_string),
                origin: entry["namespace"].as_str().map(str::to_string),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `spack list`: package names, one per line when piped (several per line
/// if colified), with `==>` status lines skipped.
fn parse_list(stdout: &str) -> Vec<SpackPackage> {
    stdout
        .lines()
        .filter(|line| !line.starts_with("==>"))
        .flat_map(str::split_whitespace)
        .map(|name| SpackPackage {
            name: name.to_string(),
            state: InstallState::Available,
            ..Default::default()
        })
        .collect()
}

/// `spack info`: sections opened by unindented `Header:` lines with
/// indented content; the first line is `<BuildSystem>Package:   <name>` and
/// `Homepage:` carries its value inline.
fn parse_info(stdout: &str) -> Option<SpackPackage> {
    let mut package = SpackPackage {
        state: InstallState::Available,
        ..Default::default()
    };
    let mut section = String::new();
    let mut dependencies: Vec<String> = Vec::new();
    for line in stdout.lines() {
        if !line.starts_with(char::is_whitespace) {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            section = key.to_string();
            if package.name.is_empty() && key.ends_with("Package") && !value.is_empty() {
                package.name = value.to_string();
            } else if key == "Homepage" && !value.is_empty() {
                package.homepage = Some(value.to_string());
            }
            continue;
        }
        let text = line.trim();
        if text.is_empty() || text == "None" {
            continue;
        }
        match section.as_str() {
            "Description" => match &mut package.description {
                Some(description) => {
                    description.push(' ');
                    description.push_str(text);
                }
                None => package.description = Some(text.to_string()),
            },
            "Preferred version" if package.version.is_none() => {
                package.version = text.split_whitespace().next().map(str::to_string);
            }
            "Licenses" if package.license.is_none() => {
                package.license = Some(text.to_string());
            }
            "Link Dependencies" | "Run Dependencies" => {
                dependencies.extend(text.split_whitespace().map(str::to_string));
            }
            _ => {}
        }
    }
    if !dependencies.is_empty() {
        dependencies.sort();
        dependencies.dedup();
        package.dependencies = Some(dependencies);
    }
    (!package.name.is_empty()).then_some(package)
}

/// A package as spack describes it.
#[derive(Debug, Default)]
pub struct SpackPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub architecture: Option<String>,
    pub origin: Option<String>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for SpackPackage {
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

    fn homepage(&self) -> Option<&str> {
        self.homepage.as_deref()
    }

    fn license(&self) -> Option<&str> {
        self.license.as_deref()
    }

    fn architecture(&self) -> Option<&str> {
        self.architecture.as_deref()
    }

    fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
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
    fn parses_find_records() {
        let json: Value = serde_json::from_str(
            r#"[
                {"name": "zlib", "version": "1.2.13", "namespace": "builtin",
                 "arch": {"platform": "linux", "platform_os": "ubuntu22.04",
                          "target": "x86_64"},
                 "compiler": {"name": "gcc", "version": "11.4.0"},
                 "hash": "abcdef1234"},
                {"name": "cmake", "version": "3.27.7", "namespace": "builtin",
                 "arch": {"platform": "linux", "platform_os": "ubuntu22.04",
                          "target": {"name": "zen2", "vendor": "AuthenticAMD"}},
                 "hash": "fedcba4321"}
            ]"#,
        )
        .unwrap();
        let packages = parse_find(&json);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "zlib");
        assert_eq!(packages[0].version.as_deref(), Some("1.2.13"));
        assert_eq!(packages[0].architecture.as_deref(), Some("x86_64"));
        assert_eq!(packages[0].origin.as_deref(), Some("builtin"));
        assert_eq!(packages[0].state, InstallState::Installed);
        // Newer spack nests the target in an object.
        assert_eq!(packages[1].architecture.as_deref(), Some("zen2"));
    }

    #[test]
    fn parses_list_names() {
        let stdout = "\
==> 3 packages
zlib
zlib-api zlib-ng
";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "zlib");
        assert_eq!(packages[1].name, "zlib-api");
        assert_eq!(packages[2].name, "zlib-ng");
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn parses_info_sections() {
        let stdout = "\
AutotoolsPackage:   zlib

Description:
    A free, general-purpose, legally unencumbered lossless
    data-compression library.

Homepage: https://zlib.net

Preferred version:
    1.2.13     https://zlib.net/fossils/zlib-1.2.13.tar.gz

Safe versions:
    1.2.13     https://zlib.net/fossils/zlib-1.2.13.tar.gz
    1.2.12     https://zlib.net/fossils/zlib-1.2.12.tar.gz

Deprecated versions:
    None

Variants:
    optimize [true]             true, false

Build Dependencies:
    gnuconfig

Link Dependencies:
    None

Run Dependencies:
    None

Licenses:
    Zlib
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.name, "zlib");
        assert_eq!(package.version.as_deref(), Some("1.2.13"));
        assert_eq!(
            package.description.as_deref(),
            Some("A free, general-purpose, legally unencumbered lossless data-compression library.")
        );
        assert_eq!(package.homepage.as_deref(), Some("https://zlib.net"));
        assert_eq!(package.license.as_deref(), Some("Zlib"));
        assert_eq!(package.dependencies, None);
    }

    #[test]
    fn info_collects_runtime_dependencies() {
        let stdout = "\
CMakePackage:   fmt

Homepage: https://fmt.dev

Link Dependencies:
    none-is-a-name
Run Dependencies:
    zlib none-is-a-name
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(
            package.dependencies,
            Some(vec!["none-is-a-name".to_string(), "zlib".to_string()])
        );
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("zlib@1.2.13")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("zlib")]).is_ok());
    }
}
