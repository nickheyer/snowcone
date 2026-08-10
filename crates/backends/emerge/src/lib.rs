//! Portage backend for snowcone.
//!
//! emerge never prompts unless `--ask` is passed (which this backend never
//! does), so `assume_yes` has nothing to toggle - mutations simply run.
//! `--pretend` is a faithful native dry run for install, remove, and
//! upgrade. Portage has no list-installed verb without gentoolkit; the
//! installed-package database IS the `/var/db/pkg/<category>/<name-version>/`
//! tree, so list-installed reads that tree directly. Removal uses
//! `--depclean`, the handbook-blessed safe route (it implies `--deselect`
//! and refuses to break dependencies), not the blunt `--unmerge`.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "emerge";
const PROGRAMS: &[&str] = &["emerge"];
const VDB: &str = "/var/db/pkg";

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

    /// Read invocation with a stable locale and colors off, so parsing
    /// survives i18n and tty detection.
    fn query(&self) -> Cmd {
        Cmd::new(&self.program).env("LC_ALL", "C").arg("--color=n")
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

    /// Shared shape for mutating commands: elevated, `--pretend` on dry run.
    fn mutation(&self, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().elevated(true);
        if ctx.dry_run {
            cmd = cmd.arg("--pretend");
        }
        cmd
    }
}

/// `=name-version` (Portage's exact-version atom) when the request pins
/// one, bare name otherwise.
fn spec(request: &PackageRequest) -> String {
    match &request.version {
        Some(version) => format!("={}-{version}", request.name),
        None => request.name.clone(),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Portage"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "portage"
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
        let cmd = self.mutation(ctx).args(packages.iter().map(spec));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = self
            .mutation(ctx)
            .arg("--depclean")
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(scan_vdb(Path::new(VDB))?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // `%` makes the search term a regex, `@` widens the match to
        // include the category (man emerge's own `%@^dev-java.*jdk` idiom).
        let pattern = match name.contains('/') {
            true => format!("%@^{}$", regex_escape(name)),
            false => format!("%^{}$", regex_escape(name)),
        };
        let output = self
            .query()
            .arg("--search")
            .arg(&pattern)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        parse_search(&output.stdout)
            .into_iter()
            .find(|package| package.name == name || package.name.rsplit('/').next() == Some(name))
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
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

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: refresh has no dry-run mode")));
        }
        self.run(self.cmd().arg("--sync").elevated(true), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let mut cmd = self.mutation(ctx).args(["--update", "--deep", "--newuse"]);
        cmd = if packages.is_empty() {
            cmd.arg("@world")
        } else {
            cmd.args(packages.iter().map(spec))
        };
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["--pretend", "--update", "--deep", "--newuse", "@world"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_pretend(&output.stdout)))
    }
}

fn boxed(packages: Vec<EmergePackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// Anchor a package name inside a regex without letting `+`, `.`, … act as
/// metacharacters.
fn regex_escape(name: &str) -> String {
    let mut escaped = String::with_capacity(name.len());
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() && c != '_' && c != '-' && c != '/' {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// `ripgrep-14.1.0-r1` → `("ripgrep", Some("14.1.0-r1"))`: the version
/// starts at the last hyphen followed by a digit (PMS forbids names ending
/// in a version-shaped suffix, and revisions are `-rN`, so that hyphen is
/// unambiguous).
fn split_pv(pf: &str) -> (String, Option<String>) {
    let mut split = None;
    for (idx, _) in pf.match_indices('-') {
        if pf[idx + 1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
        {
            split = Some(idx);
        }
    }
    match split {
        Some(idx) => (pf[..idx].to_string(), Some(pf[idx + 1..].to_string())),
        None => (pf.to_string(), None),
    }
}

/// Read Portage's installed-package database: every
/// `<root>/<category>/<name-version>` directory is one installed package.
/// In-progress merges are staged as `-MERGING-…` directories and skipped.
fn scan_vdb(root: &Path) -> std::io::Result<Vec<EmergePackage>> {
    let mut packages = Vec::new();
    for category in std::fs::read_dir(root)? {
        let category = category?;
        if !category.file_type()?.is_dir() {
            continue;
        }
        let category_name = category.file_name();
        let Some(category_name) = category_name.to_str() else {
            continue;
        };
        if category_name.starts_with('.') || category_name.starts_with('-') {
            continue;
        }
        for entry in std::fs::read_dir(category.path())? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let pf = entry.file_name();
            let Some(pf) = pf.to_str() else {
                continue;
            };
            if pf.starts_with('.') || pf.starts_with('-') {
                continue;
            }
            let (name, version) = split_pv(pf);
            packages.push(EmergePackage {
                name: format!("{category_name}/{name}"),
                version,
                state: InstallState::Installed,
                ..Default::default()
            });
        }
    }
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(packages)
}

/// `emerge --search` blocks: a `*  category/name` header (possibly tagged
/// `[ Masked ]`) followed by indented `Key: Value` lines; a missing install
/// shows as `Latest version installed: [ Not Installed ]`.
fn parse_search(stdout: &str) -> Vec<EmergePackage> {
    struct Block {
        package: EmergePackage,
        installed: Option<String>,
        available: Option<String>,
    }

    fn finish(block: Option<Block>, packages: &mut Vec<EmergePackage>) {
        let Some(Block {
            mut package,
            installed,
            available,
        }) = block
        else {
            return;
        };
        match installed {
            Some(version) => {
                if available.as_ref().is_some_and(|latest| *latest != version) {
                    package.latest_version = available;
                    package.state = InstallState::Upgradable;
                } else {
                    package.state = InstallState::Installed;
                }
                package.version = Some(version);
            }
            None => {
                package.version = available;
                package.state = InstallState::Available;
            }
        }
        if !package.name.is_empty() {
            packages.push(package);
        }
    }

    let mut packages = Vec::new();
    let mut current: Option<Block> = None;
    for line in stdout.lines() {
        if let Some(header) = line.strip_prefix('*') {
            finish(current.take(), &mut packages);
            let Some(name) = header.split_whitespace().next() else {
                continue;
            };
            current = Some(Block {
                package: EmergePackage {
                    name: name.to_string(),
                    ..Default::default()
                },
                installed: None,
                available: None,
            });
            continue;
        }
        let Some(block) = current.as_mut() else {
            continue;
        };
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if value.is_empty() {
            continue;
        }
        match key {
            "Latest version available" => block.available = Some(value.to_string()),
            // `[ Not Installed ]` (and `[ Masked ]`-style tags) start with
            // a bracket; a real version never does.
            "Latest version installed" if !value.starts_with('[') => {
                block.installed = Some(value.to_string());
            }
            "Size of files" => block.package.download_size = parse_size(value),
            "Homepage" => block.package.homepage = Some(value.to_string()),
            "Description" => block.package.description = Some(value.to_string()),
            "License" => block.package.license = Some(value.to_string()),
            _ => {}
        }
    }
    finish(current, &mut packages);
    packages
}

/// `2,352 KiB`-style sizes: comma-grouped number plus a 1024-based unit.
fn parse_size(value: &str) -> Option<u64> {
    let mut parts = value.split_whitespace();
    let number: f64 = parts.next()?.replace(',', "").parse().ok()?;
    let factor = match parts.next().unwrap_or("B") {
        "B" | "bytes" => 1.0,
        "KiB" | "KB" | "kB" => 1024.0,
        "MiB" | "MB" => 1024.0 * 1024.0,
        "GiB" | "GB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((number * factor) as u64)
}

/// `emerge --pretend` merge lines: `[ebuild  U  ] category/name-version
/// [old-version] …`; only lines whose flag field contains `U` (replacing an
/// installed version) are upgrades.
fn parse_pretend(stdout: &str) -> Vec<EmergePackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let rest = line
                .strip_prefix("[ebuild")
                .or_else(|| line.strip_prefix("[binary"))?;
            let (flags, rest) = rest.split_once(']')?;
            if !flags.contains('U') {
                return None;
            }
            let mut parts = rest.split_whitespace();
            let atom = parts.next()?;
            let (category, pf) = atom.split_once('/')?;
            let (name, version) = split_pv(pf);
            let installed = parts.next().and_then(|token| {
                let inner = token.strip_prefix('[')?.strip_suffix(']')?;
                inner
                    .chars()
                    .next()?
                    .is_ascii_digit()
                    .then(|| inner.to_string())
            });
            Some(EmergePackage {
                name: format!("{category}/{name}"),
                version: installed,
                latest_version: version,
                state: InstallState::Upgradable,
                ..Default::default()
            })
        })
        .collect()
}

/// A package as Portage describes it.
#[derive(Debug, Default)]
pub struct EmergePackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub download_size: Option<u64>,
    pub state: InstallState,
}

impl Package for EmergePackage {
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

    fn download_size(&self) -> Option<u64> {
        self.download_size
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_pf_into_name_and_version() {
        assert_eq!(
            split_pv("ripgrep-14.1.0"),
            ("ripgrep".to_string(), Some("14.1.0".to_string()))
        );
        assert_eq!(
            split_pv("portage-3.0.66.1-r1"),
            ("portage".to_string(), Some("3.0.66.1-r1".to_string()))
        );
        // Digit-leading name chunks stay in the name.
        assert_eq!(
            split_pv("font-adobe-100dpi-1.0.4"),
            ("font-adobe-100dpi".to_string(), Some("1.0.4".to_string()))
        );
        assert_eq!(split_pv("gtk+extra"), ("gtk+extra".to_string(), None));
    }

    #[test]
    fn parses_search_blocks() {
        let stdout = "\
[ Results for search key : ripgrep ]
Searching...

*  sys-apps/ripgrep
      Latest version available: 14.1.0
      Latest version installed: [ Not Installed ]
      Size of files: 2,352 KiB
      Homepage:      https://github.com/BurntSushi/ripgrep
      Description:   Search tool that combines the usability of ag with the raw speed of grep
      License:       Apache-2.0 MIT UoI-NCSA

*  sys-apps/the_silver_searcher
      Latest version available: 2.2.0-r2
      Latest version installed: 2.2.0-r2
      Size of files: 158 KiB
      Homepage:      https://geoff.greer.fm/ag/
      Description:   A code-searching tool similar to ack, but faster
      License:       Apache-2.0

[ Applications found : 2 ]
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "sys-apps/ripgrep");
        assert_eq!(packages[0].version.as_deref(), Some("14.1.0"));
        assert_eq!(packages[0].state, InstallState::Available);
        assert_eq!(packages[0].download_size, Some(2352 * 1024));
        assert_eq!(
            packages[0].homepage.as_deref(),
            Some("https://github.com/BurntSushi/ripgrep")
        );
        assert_eq!(packages[1].state, InstallState::Installed);
        assert_eq!(packages[1].version.as_deref(), Some("2.2.0-r2"));
    }

    #[test]
    fn search_marks_upgradable_installs() {
        let stdout = "\
*  sys-apps/ripgrep
      Latest version available: 14.1.0
      Latest version installed: 13.0.0
      Description:   Search tool
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].version.as_deref(), Some("13.0.0"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("14.1.0"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn parses_pretend_upgrade_lines() {
        let stdout = "\
These are the packages that would be merged, in order:

Calculating dependencies... done!
[ebuild     U  ] sys-apps/portage-3.0.66.1 [3.0.65] USE=\"rsync-verify\"
[ebuild  N     ] dev-libs/libffi-3.4.6
[ebuild   R    ] sys-apps/sandbox-2.38
[blocks b      ] <sys-apps/openrc-0.54

Total: 3 packages (1 upgrade, 1 new, 1 reinstall)
";
        let packages = parse_pretend(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "sys-apps/portage");
        assert_eq!(packages[0].version.as_deref(), Some("3.0.65"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("3.0.66.1"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("2,352 KiB"), Some(2352 * 1024));
        assert_eq!(parse_size("158 KiB"), Some(158 * 1024));
        assert_eq!(parse_size("1 MiB"), Some(1024 * 1024));
        assert_eq!(parse_size("weird"), None);
    }

    #[test]
    fn scans_the_vdb_tree() {
        let root = std::env::temp_dir().join(format!("snowcone-emerge-vdb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sys-apps/ripgrep-14.1.0")).unwrap();
        std::fs::create_dir_all(root.join("sys-apps/-MERGING-portage-3.0.66.1")).unwrap();
        std::fs::create_dir_all(root.join("app-shells/bash-5.2_p26-r6")).unwrap();
        std::fs::write(root.join("sys-apps/ripgrep-14.1.0/SLOT"), "0\n").unwrap();
        let packages = scan_vdb(&root).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "app-shells/bash");
        assert_eq!(packages[0].version.as_deref(), Some("5.2_p26-r6"));
        assert_eq!(packages[1].name, "sys-apps/ripgrep");
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("sys-apps/ripgrep@14.1.0")),
            "=sys-apps/ripgrep-14.1.0"
        );
        assert_eq!(spec(&PackageRequest::parse("ripgrep")), "ripgrep");
    }

    #[test]
    fn escapes_regex_metacharacters() {
        assert_eq!(regex_escape("gtk+"), "gtk\\+");
        assert_eq!(regex_escape("sys-apps/ripgrep"), "sys-apps/ripgrep");
    }
}
