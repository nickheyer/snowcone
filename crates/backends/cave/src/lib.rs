//! Paludis (cave) backend for snowcone.
//!
//! `cave resolve` and `cave uninstall` are their own dry run: without
//! `-x`/`--execute` they only display the resolution, so `dry_run` simply
//! drops the `-x`. cave asks no y/n questions (the no-`-x` display is the
//! confirmation step), leaving `assume_yes` nothing to do. The installed
//! set comes from `cave print-ids` matching `*/*::/` - the
//! scripting-oriented listing of everything installed to /. The display
//! commands (`search`, `show`, a no-execute `resolve`) are not stable
//! scripting interfaces, so their parsers are deliberately tolerant.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "cave";
const PROGRAMS: &[&str] = &["cave"];

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

    /// `resolve`/`uninstall` with `-x` unless this is a dry run - omitting
    /// `-x` is cave's native "display only" mode.
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(subcommand).elevated(true);
        if !ctx.dry_run {
            cmd = cmd.arg("-x");
        }
        cmd
    }

    /// Everything installed to /, via the scripting-friendly print command.
    async fn installed(&self) -> Result<Vec<CavePackage>> {
        let output = self
            .query()
            .args(["print-ids", "-m", "*/*::/", "-f", "%c/%p %v\\n"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_print_ids(&output.stdout))
    }
}

/// `=name-version` (Paludis' exact-version spec) when the request pins one,
/// bare name otherwise.
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
        "Paludis"
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
        let cmd = self
            .mutation("resolve", ctx)
            .args(packages.iter().map(spec));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = self
            .mutation("uninstall", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.installed().await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .query()
            .arg("show")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        let mut package =
            parse_show(&output.stdout).ok_or_else(|| Error::NotFound(name.to_string()))?;
        // `show` lists the ::installed instance when there is one, but a
        // print-ids probe is authoritative if that header was missing.
        if package.state != InstallState::Installed
            && package.state != InstallState::Upgradable
            && let Some(installed) = self.installed().await?.into_iter().find(|installed| {
                installed.name == package.name
                    || installed.name.rsplit('/').next() == Some(package.name.as_str())
            })
        {
            package.state = InstallState::Installed;
            if installed.version.is_some() && installed.version != package.version {
                package.latest_version = package.version.take();
                package.version = installed.version;
                package.state = InstallState::Upgradable;
            }
        }
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .arg("search")
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
        self.run(self.cmd().arg("sync").elevated(true), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let cmd = if packages.is_empty() {
            self.mutation("resolve", ctx).arg("world")
        } else {
            self.mutation("resolve", ctx)
                .args(packages.iter().map(spec))
        };
        self.run(cmd, ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        // A no-execute resolution of world is cave's only upgrade preview.
        let output = self
            .query()
            .args(["resolve", "world"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_resolution(&output.stdout)))
    }
}

fn boxed(packages: Vec<CavePackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `name-1.2.3-r1` → `("name", Some("1.2.3-r1"))`: the version starts at
/// the last hyphen followed by a digit (revision suffixes are `-rN`).
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

/// `category/name-version:slot::repo` → (`category/name`, version, repo);
/// version and slot are optional in the id.
fn parse_id(token: &str) -> Option<(String, Option<String>, Option<String>)> {
    let (left, repo) = match token.split_once("::") {
        Some((left, repo)) => (left, Some(repo.to_string())),
        None => (token, None),
    };
    let left = left.split(':').next().unwrap_or(left);
    let (category, pf) = left.split_once('/')?;
    let (name, version) = split_pv(pf);
    Some((format!("{category}/{name}"), version, repo))
}

/// `cave print-ids -f '%c/%p %v\n'`: one `category/name version` pair per
/// line.
fn parse_print_ids(stdout: &str) -> Vec<CavePackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            if !name.contains('/') {
                return None;
            }
            Some(CavePackage {
                name: name.to_string(),
                version: parts.next().map(str::to_string),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `cave search`: unindented `category/name` headers (some versions prefix
/// `* `), each followed by indented `::repo version…` lines and a quoted or
/// plain description line.
fn parse_search(stdout: &str) -> Vec<CavePackage> {
    struct Block {
        package: CavePackage,
        installed: Option<String>,
        available: Option<String>,
    }

    fn finish(block: Option<Block>, packages: &mut Vec<CavePackage>) {
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
        packages.push(package);
    }

    let mut packages = Vec::new();
    let mut current: Option<Block> = None;
    for line in stdout.lines() {
        if !line.starts_with(char::is_whitespace) {
            let header = line.trim_start_matches('*').trim();
            let Some(name) = header.split_whitespace().next() else {
                continue;
            };
            if !name.contains('/') {
                continue;
            }
            finish(current.take(), &mut packages);
            current = Some(Block {
                package: CavePackage {
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
        let text = line.trim();
        if let Some(rest) = text.strip_prefix("::") {
            let mut parts = rest.split_whitespace();
            let repo = parts.next().unwrap_or("");
            let version = parts
                .find(|token| token.chars().next().is_some_and(|c| c.is_ascii_digit()))
                .map(|token| token.trim_end_matches('*').to_string());
            if repo.contains("installed") {
                block.installed = version;
            } else {
                if block.package.origin.is_none() {
                    block.package.origin = Some(repo.to_string());
                }
                if version.is_some() {
                    block.available = version;
                }
            }
        } else if !text.is_empty() && block.package.description.is_none() {
            block.package.description = Some(text.trim_matches('"').to_string());
        }
    }
    finish(current, &mut packages);
    packages
}

/// `cave show`: `category/name-version:slot::repo` id headers with
/// indented `Key    Value` metadata lines (two-plus spaces between); the
/// `::installed` id marks the installed instance. Paludis spells it
/// "Licences".
fn parse_show(stdout: &str) -> Option<CavePackage> {
    let mut package = CavePackage::default();
    let mut installed: Option<String> = None;
    let mut available: Option<String> = None;
    for line in stdout.lines() {
        if !line.starts_with(char::is_whitespace) {
            let header = line.trim_start_matches('*').trim();
            let Some(token) = header.split_whitespace().next() else {
                continue;
            };
            let Some((name, version, repo)) = parse_id(token) else {
                continue;
            };
            if package.name.is_empty() {
                package.name = name;
            } else if package.name != name {
                continue;
            }
            if repo
                .as_deref()
                .is_some_and(|repo| repo.contains("installed"))
            {
                installed = version.or(installed.take());
            } else {
                if version.is_some() {
                    available = version;
                }
                if package.origin.is_none() {
                    package.origin = repo;
                }
            }
            continue;
        }
        let text = line.trim();
        let Some((key, value)) = text.split_once("  ") else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if value.is_empty() {
            continue;
        }
        match key {
            "Summary" => package.description = Some(value.to_string()),
            "Description" if package.description.is_none() => {
                package.description = Some(value.to_string());
            }
            "Homepage" | "Homepages" => package.homepage = Some(value.to_string()),
            "Licences" | "Licenses" | "License" => package.license = Some(value.to_string()),
            _ => {}
        }
    }
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
    (!package.name.is_empty()).then_some(package)
}

/// Upgrade entries from a no-execute `cave resolve world` display: any line
/// whose id token is followed by `old -> new` or `… replacing old`.
/// Best-effort - cave's resolution display is not a stable format.
fn parse_resolution(stdout: &str) -> Vec<CavePackage> {
    let looks_like_version = |token: &str| token.chars().next().is_some_and(|c| c.is_ascii_digit());
    let mut packages = Vec::new();
    for line in stdout.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let Some(id_at) = tokens.iter().position(|token| token.contains('/')) else {
            continue;
        };
        let Some((name, id_version, _)) = parse_id(tokens[id_at]) else {
            continue;
        };
        let old;
        let mut new = id_version;
        if let Some(arrow) = tokens.iter().position(|&token| token == "->") {
            old = arrow
                .checked_sub(1)
                .and_then(|at| tokens.get(at))
                .filter(|token| looks_like_version(token))
                .map(|token| token.to_string());
            new = tokens
                .get(arrow + 1)
                .filter(|token| looks_like_version(token))
                .map(|token| token.to_string())
                .or(new);
        } else if let Some(replacing) = tokens.iter().position(|&token| token == "replacing") {
            old = tokens
                .get(replacing + 1)
                .map(|token| token.trim_end_matches(','))
                .filter(|token| looks_like_version(token))
                .map(str::to_string);
            if new.is_none() {
                new = tokens
                    .get(id_at + 1)
                    .filter(|token| looks_like_version(token))
                    .map(|token| token.to_string());
            }
        } else {
            continue;
        }
        let (Some(old), Some(new)) = (old, new) else {
            continue;
        };
        packages.push(CavePackage {
            name,
            version: Some(old),
            latest_version: Some(new),
            state: InstallState::Upgradable,
            ..Default::default()
        });
    }
    packages
}

/// A package as Paludis describes it.
#[derive(Debug, Default)]
pub struct CavePackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub origin: Option<String>,
    pub state: InstallState,
}

impl Package for CavePackage {
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

    fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_print_ids_lines() {
        let stdout = "\
app-shells/bash 5.2.21
sys-apps/ripgrep 14.1.0
";
        let packages = parse_print_ids(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "sys-apps/ripgrep");
        assert_eq!(packages[1].version.as_deref(), Some("14.1.0"));
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn parses_id_tokens() {
        assert_eq!(
            parse_id("sys-apps/ripgrep-14.1.0:0::arbor"),
            Some((
                "sys-apps/ripgrep".to_string(),
                Some("14.1.0".to_string()),
                Some("arbor".to_string())
            ))
        );
        assert_eq!(
            parse_id("sys-apps/ripgrep"),
            Some(("sys-apps/ripgrep".to_string(), None, None))
        );
    }

    #[test]
    fn parses_search_blocks() {
        let stdout = "\
* sys-apps/ripgrep
    ::arbor                   14.1.0 {:0}
    \"Recursively search directories for a regex pattern\"

* app-shells/zsh
    ::installed               5.9
    ::arbor                   5.9 {:0}
    Shell designed for interactive use
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "sys-apps/ripgrep");
        assert_eq!(packages[0].version.as_deref(), Some("14.1.0"));
        assert_eq!(packages[0].origin.as_deref(), Some("arbor"));
        assert_eq!(packages[0].state, InstallState::Available);
        assert_eq!(
            packages[0].description.as_deref(),
            Some("Recursively search directories for a regex pattern")
        );
        assert_eq!(packages[1].state, InstallState::Installed);
        assert_eq!(packages[1].version.as_deref(), Some("5.9"));
    }

    #[test]
    fn parses_show_with_installed_and_repo_ids() {
        let stdout = "\
sys-apps/ripgrep-13.0.0:0::installed
    Summary                   Recursively search directories for a regex pattern
    Homepage                  https://github.com/BurntSushi/ripgrep
    Licences                  MIT Unlicense

sys-apps/ripgrep-14.1.0:0::arbor
    Summary                   Recursively search directories for a regex pattern
";
        let package = parse_show(stdout).unwrap();
        assert_eq!(package.name, "sys-apps/ripgrep");
        assert_eq!(package.version.as_deref(), Some("13.0.0"));
        assert_eq!(package.latest_version.as_deref(), Some("14.1.0"));
        assert_eq!(package.state, InstallState::Upgradable);
        assert_eq!(package.license.as_deref(), Some("MIT Unlicense"));
        assert_eq!(package.origin.as_deref(), Some("arbor"));
    }

    #[test]
    fn parses_show_without_install() {
        let stdout = "\
sys-apps/ripgrep-14.1.0:0::arbor
    Summary                   Recursively search directories for a regex pattern
";
        let package = parse_show(stdout).unwrap();
        assert_eq!(package.version.as_deref(), Some("14.1.0"));
        assert_eq!(package.state, InstallState::Available);
    }

    #[test]
    fn parses_resolution_upgrades() {
        let stdout = "\
These are the actions I will take, in order:

u   sys-apps/coreutils:0::arbor 9.4 -> 9.5
n   dev-libs/pcre2:0::arbor 10.43
*   sys-apps/ripgrep-14.1.0:0::arbor replacing 13.0.0

Total: 3 packages
";
        let packages = parse_resolution(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "sys-apps/coreutils");
        assert_eq!(packages[0].version.as_deref(), Some("9.4"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("9.5"));
        assert_eq!(packages[1].name, "sys-apps/ripgrep");
        assert_eq!(packages[1].version.as_deref(), Some("13.0.0"));
        assert_eq!(packages[1].latest_version.as_deref(), Some("14.1.0"));
        assert_eq!(packages[1].state, InstallState::Upgradable);
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("sys-apps/ripgrep@14.1.0")),
            "=sys-apps/ripgrep-14.1.0"
        );
        assert_eq!(spec(&PackageRequest::parse("ripgrep")), "ripgrep");
    }
}
