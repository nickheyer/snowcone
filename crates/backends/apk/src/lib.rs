//! apk-tools backend for snowcone.
//!
//! Alpine's apk is non-interactive by design - it never prompts unless
//! `-i/--interactive` is passed, so `assume_yes` has nothing to do. add,
//! del, and upgrade take a native `--simulate`; the index refresh does not.
//! Mutations run through the elevation helper. apk speaks in
//! `name-version-rN` strings throughout, split at the last dash that is
//! followed by a digit.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "apk";
const PROGRAMS: &[&str] = &["apk"];

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

    /// Elevated mutating subcommand with the native simulate switch.
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(subcommand).elevated(true);
        if ctx.dry_run {
            cmd = cmd.arg("--simulate");
        }
        cmd
    }
}

/// `name=version` when the request pins one, bare name otherwise.
fn spec(request: &PackageRequest) -> String {
    match &request.version {
        Some(version) => format!("{}={version}", request.name),
        None => request.name.clone(),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "apk-tools"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "apkdb"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::REFRESH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
            | Capabilities::PIN_VERSION
    }

    fn needs_elevation(&self, operation: Operation) -> bool {
        matches!(
            operation,
            Operation::Install | Operation::Remove | Operation::Upgrade | Operation::Refresh
        )
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = self.mutation("add", ctx).args(packages.iter().map(spec));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = self
            .mutation("del", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["list", "--installed"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_list(&output.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // `apk list` carries version/arch/origin/license and the install
        // markers; `apk info` adds description, webpage, and depends.
        let list = self
            .query()
            .arg("list")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        let mut package = parse_list(&list.stdout)
            .into_iter()
            .filter(|package| package.name == name)
            .max_by_key(|package| package.state != InstallState::Available);
        let details = self
            .query()
            .args(["info", "-d", "-w", "-R"])
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if let Some(described) = parse_info(&details.stdout).filter(|d| d.name == name) {
            match &mut package {
                Some(package) => {
                    package.description = described.description;
                    package.homepage = described.homepage;
                    package.dependencies = described.dependencies;
                }
                None => package = Some(described),
            }
        }
        package
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["search", "-v"])
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_search(&output.stdout)))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: refresh has no dry-run mode")));
        }
        self.run(self.cmd().arg("update").elevated(true), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = if packages.is_empty() {
            self.mutation("upgrade", ctx)
        } else {
            // `add --upgrade` moves just the named packages (pins included)
            // where bare `upgrade` would only honor installed names.
            self.mutation("add", ctx)
                .arg("--upgrade")
                .args(packages.iter().map(spec))
        };
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["list", "--upgradable"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_list(&output.stdout)))
    }
}

fn boxed(packages: Vec<ApkPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `name-1.2.3-r0` → `("name", Some("1.2.3-r0"))`: the version starts at
/// the last dash followed by a digit (names like `py3-foo` contain dashes,
/// and the `-rN` release suffix never starts with one).
fn split_pkgver(pkgver: &str) -> (String, Option<String>) {
    for (idx, _) in pkgver.match_indices('-').rev() {
        if pkgver[idx + 1..].starts_with(|c: char| c.is_ascii_digit()) {
            return (
                pkgver[..idx].to_string(),
                Some(pkgver[idx + 1..].to_string()),
            );
        }
    }
    (pkgver.to_string(), None)
}

/// `apk list`: `pkgver arch {origin} (license) [markers]` lines, where the
/// license may contain spaces and `[upgradable from: pkgver]` names the
/// installed version.
fn parse_list(stdout: &str) -> Vec<ApkPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let (name, version) = split_pkgver(parts.next()?);
            version.as_ref()?;
            let mut package = ApkPackage {
                name,
                version,
                architecture: parts.next().map(str::to_string),
                origin: between(line, '{', '}').map(str::to_string),
                license: between(line, '(', ')').map(str::to_string),
                state: InstallState::Available,
                ..Default::default()
            };
            if let Some((_, from)) = line.split_once("[upgradable from: ") {
                let (_, current) = split_pkgver(from.trim_end_matches(']'));
                package.latest_version = package.version.take();
                package.version = current;
                package.state = InstallState::Upgradable;
            } else if line.contains("[installed") {
                package.state = InstallState::Installed;
            }
            Some(package)
        })
        .collect()
}

/// The text between the first `open` and the last `close` in `line`.
fn between(line: &str, open: char, close: char) -> Option<&str> {
    let start = line.find(open)? + open.len_utf8();
    let end = line.rfind(close)?;
    (start <= end).then(|| &line[start..end])
}

/// `apk search -v`: `pkgver - description` lines; the index has no install
/// state, so everything parses as available.
fn parse_search(stdout: &str) -> Vec<ApkPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let (pkgver, description) = match line.split_once(" - ") {
                Some((pkgver, description)) => (pkgver, Some(description.trim())),
                None => (line.trim(), None),
            };
            let (name, version) = split_pkgver(pkgver);
            version.as_ref()?;
            Some(ApkPackage {
                name,
                version,
                description: description
                    .filter(|text| !text.is_empty())
                    .map(str::to_string),
                state: InstallState::Available,
                ..Default::default()
            })
        })
        .collect()
}

/// `apk info -d -w -R`: `<pkgver> <label>:` section headers, each followed
/// by the section's text and terminated by a blank line.
fn parse_info(stdout: &str) -> Option<ApkPackage> {
    let mut package: Option<ApkPackage> = None;
    let mut section: Option<String> = None;
    for line in stdout.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            section = None;
            continue;
        }
        if let Some(header) = line.strip_suffix(':')
            && let Some((pkgver, label)) = header.split_once(' ')
            && let (name, Some(version)) = split_pkgver(pkgver)
        {
            package.get_or_insert_with(|| ApkPackage {
                name,
                version: Some(version),
                state: InstallState::Available,
                ..Default::default()
            });
            section = Some(label.to_string());
            continue;
        }
        let Some(entry) = &mut package else { continue };
        match section.as_deref() {
            Some("description") => match &mut entry.description {
                Some(description) => {
                    description.push(' ');
                    description.push_str(line);
                }
                None => entry.description = Some(line.to_string()),
            },
            Some("webpage") if entry.homepage.is_none() => {
                entry.homepage = Some(line.to_string());
            }
            Some("depends on") => {
                entry
                    .dependencies
                    .get_or_insert_with(Vec::new)
                    .push(line.to_string());
            }
            _ => {}
        }
    }
    package
}

/// A package as apk describes it.
#[derive(Debug, Default)]
pub struct ApkPackage {
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

impl Package for ApkPackage {
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
    fn splits_pkgver_strings() {
        assert_eq!(
            split_pkgver("zlib-1.3.1-r1"),
            ("zlib".to_string(), Some("1.3.1-r1".to_string()))
        );
        assert_eq!(
            split_pkgver("py3-requests-2.31.0-r0"),
            ("py3-requests".to_string(), Some("2.31.0-r0".to_string()))
        );
        assert_eq!(split_pkgver("busybox"), ("busybox".to_string(), None));
    }

    #[test]
    fn parses_installed_list_lines() {
        let stdout = "\
musl-1.2.5-r0 x86_64 {musl} (MIT) [installed]
zlib-1.3.1-r1 x86_64 {zlib} (Zlib) [installed]
";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "zlib");
        assert_eq!(packages[1].version.as_deref(), Some("1.3.1-r1"));
        assert_eq!(packages[1].architecture.as_deref(), Some("x86_64"));
        assert_eq!(packages[1].origin.as_deref(), Some("zlib"));
        assert_eq!(packages[1].license.as_deref(), Some("Zlib"));
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn parses_upgradable_list_lines() {
        let stdout = "busybox-1.36.1-r7 x86_64 {busybox} (GPL-2.0-only) \
[upgradable from: busybox-1.36.1-r5]\n";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "busybox");
        assert_eq!(packages[0].version.as_deref(), Some("1.36.1-r5"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("1.36.1-r7"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn parses_spaced_license_and_bare_lines() {
        let packages = parse_list("ripgrep-14.1.0-r0 x86_64 {ripgrep} (MIT UNLICENSE)\n");
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].license.as_deref(), Some("MIT UNLICENSE"));
        assert_eq!(packages[0].state, InstallState::Available);
    }

    #[test]
    fn parses_search_entries() {
        let stdout = "\
ripgrep-14.1.0-r0 - recursively search directories for a regex pattern
ripgrep-doc-14.1.0-r0 - recursively search directories for a regex pattern (documentation)
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "ripgrep");
        assert_eq!(packages[0].version.as_deref(), Some("14.1.0-r0"));
        assert_eq!(
            packages[0].description.as_deref(),
            Some("recursively search directories for a regex pattern")
        );
        assert_eq!(packages[1].name, "ripgrep-doc");
        assert_eq!(packages[1].state, InstallState::Available);
    }

    #[test]
    fn parses_info_sections() {
        let stdout = "\
zlib-1.3.1-r1 description:
A compression/decompression Library

zlib-1.3.1-r1 webpage:
https://zlib.net/

zlib-1.3.1-r1 depends on:
so:libc.musl-x86_64.so.1
";
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.name, "zlib");
        assert_eq!(package.version.as_deref(), Some("1.3.1-r1"));
        assert_eq!(
            package.description.as_deref(),
            Some("A compression/decompression Library")
        );
        assert_eq!(package.homepage.as_deref(), Some("https://zlib.net/"));
        assert_eq!(
            package.dependencies,
            Some(vec!["so:libc.musl-x86_64.so.1".to_string()])
        );
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("zlib@1.3.1-r1")),
            "zlib=1.3.1-r1"
        );
        assert_eq!(spec(&PackageRequest::parse("zlib")), "zlib");
    }
}
