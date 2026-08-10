//! LuaRocks backend for snowcone.
//!
//! Targets the per-user rock tree: the stub wires no elevation, so every
//! mutation passes `--local` rather than touching the system tree. Reads
//! span all configured trees, so system-installed rocks still show up.
//! `--porcelain` gives locale-stable tab-separated output on the list,
//! search, and outdated verbs; only `show` is line-parsed. LuaRocks fetches
//! the manifest per operation (no refresh verb), keeps rock versions side
//! by side, never prompts, and has no upgrade or dry-run mode - upgrading
//! reinstalls the newest version, one rock per invocation.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "luarocks";
const PROGRAMS: &[&str] = &["luarocks"];

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

    /// Rocks with a newer manifest version, via the porcelain outdated
    /// listing (hits the remote manifest, like every LuaRocks lookup).
    async fn outdated(&self) -> Result<Vec<LuarocksPackage>> {
        let output = self
            .query()
            .args(["list", "--outdated", "--porcelain"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_outdated(&output.stdout))
    }
}

/// `install` takes a pinned version as a separate positional argument
/// after the rock name.
fn spec(request: &PackageRequest) -> Vec<String> {
    let mut args = vec![request.name.clone()];
    if let Some(version) = &request.version {
        args.push(version.clone());
    }
    args
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "LuaRocks"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "luarocks"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        for package in packages {
            let cmd = self.cmd().args(["install", "--local"]).args(spec(package));
            self.run(cmd, ctx).await?;
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        for package in packages {
            let cmd = self.cmd().args(["remove", "--local"]).arg(&package.name);
            self.run(cmd, ctx).await?;
        }
        Ok(())
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["list", "--porcelain"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_list(&output.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // `show` only knows installed rocks; a search fills in the rest.
        let show = self
            .query()
            .arg("show")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if show.success()
            && let Some(mut package) = parse_show(&show.stdout)
        {
            // The outdated probe needs the network; failure just leaves the
            // state at plain installed.
            let outdated = self
                .query()
                .args(["list", "--outdated", "--porcelain"])
                .arg(&package.name)
                .capture(&self.elevator, None)
                .await?;
            if outdated.success()
                && let Some(newer) = parse_outdated(&outdated.stdout)
                    .into_iter()
                    .find(|newer| newer.name == package.name)
            {
                package.state = InstallState::Upgradable;
                package.latest_version = newer.latest_version;
            }
            return Ok(Box::new(package));
        }
        let search = self
            .query()
            .args(["search", "--porcelain"])
            .arg(name)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        parse_search(&search.stdout)
            .into_iter()
            .find(|package| package.name == name)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let output = self
            .query()
            .args(["search", "--porcelain"])
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_search(&output.stdout)))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        // No upgrade verb: installing again picks up the newest version.
        if packages.is_empty() {
            for outdated in self.outdated().await? {
                let cmd = self.cmd().args(["install", "--local"]).arg(&outdated.name);
                self.run(cmd, ctx).await?;
            }
            return Ok(());
        }
        for package in packages {
            let cmd = self.cmd().args(["install", "--local"]).args(spec(package));
            self.run(cmd, ctx).await?;
        }
        Ok(())
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.outdated().await?))
    }
}

fn boxed(packages: Vec<LuarocksPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// `list --porcelain`: `name<TAB>version<TAB>status<TAB>tree` lines, one
/// per installed version of a rock (LuaRocks keeps versions side by side).
fn parse_list(stdout: &str) -> Vec<LuarocksPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?.trim();
            let version = fields.next()?.trim();
            if name.is_empty() || version.is_empty() {
                return None;
            }
            fields.next(); // status, always "installed" here
            Some(LuarocksPackage {
                name: name.to_string(),
                version: Some(version.to_string()),
                origin: fields.next().map(|tree| tree.trim().to_string()),
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `search --porcelain`: `name<TAB>version<TAB>arch<TAB>repo` lines, one
/// per rockspec/src/binary variant with the newest version first - the
/// first line per rock wins.
fn parse_search(stdout: &str) -> Vec<LuarocksPackage> {
    let mut packages: Vec<LuarocksPackage> = Vec::new();
    for line in stdout.lines() {
        let mut fields = line.split('\t');
        let Some(name) = fields.next().map(str::trim) else {
            continue;
        };
        let Some(version) = fields.next().map(str::trim) else {
            continue;
        };
        if name.is_empty() || version.is_empty() {
            continue;
        }
        if packages.iter().any(|seen| seen.name == name) {
            continue;
        }
        fields.next(); // arch: rockspec/src/all, not a CPU architecture
        packages.push(LuarocksPackage {
            name: name.to_string(),
            version: Some(version.to_string()),
            origin: fields.next().map(|repo| repo.trim().to_string()),
            state: InstallState::Available,
            ..Default::default()
        });
    }
    packages
}

/// `list --outdated --porcelain`: `name<TAB>installed<TAB>latest<TAB>repo`
/// lines.
fn parse_outdated(stdout: &str) -> Vec<LuarocksPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?.trim();
            let installed = fields.next()?.trim();
            let latest = fields.next()?.trim();
            if name.is_empty() || installed.is_empty() || latest.is_empty() {
                return None;
            }
            Some(LuarocksPackage {
                name: name.to_string(),
                version: Some(installed.to_string()),
                latest_version: Some(latest.to_string()),
                origin: fields.next().map(|repo| repo.trim().to_string()),
                state: InstallState::Upgradable,
                ..Default::default()
            })
        })
        .collect()
}

/// `show`: a `name version - summary` header, a free-text paragraph, then
/// `Key: Value` fields; the `Depends on:` section lists one indented
/// `name constraint` entry per line.
fn parse_show(stdout: &str) -> Option<LuarocksPackage> {
    let mut lines = stdout.lines().skip_while(|line| line.trim().is_empty());
    let header = lines.next()?;
    let (rock, summary) = match header.split_once(" - ") {
        Some((rock, summary)) => (rock, Some(summary.trim())),
        None => (header, None),
    };
    let mut parts = rock.split_whitespace();
    let mut package = LuarocksPackage {
        name: parts.next()?.to_string(),
        version: parts.next().map(str::to_string),
        description: summary.filter(|text| !text.is_empty()).map(str::to_string),
        state: InstallState::Installed,
        ..Default::default()
    };
    let mut in_depends = false;
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            in_depends = false;
            continue;
        }
        if line.starts_with(char::is_whitespace) {
            if in_depends && let Some(dep) = trimmed.split_whitespace().next() {
                package
                    .dependencies
                    .get_or_insert_with(Vec::new)
                    .push(dep.to_string());
            }
            continue;
        }
        in_depends = false;
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "License" if !value.is_empty() => package.license = Some(value.to_string()),
            "Homepage" if !value.is_empty() => package.homepage = Some(value.to_string()),
            "Installed in" if !value.is_empty() => package.origin = Some(value.to_string()),
            "Depends on" => in_depends = true,
            _ => {}
        }
    }
    Some(package)
}

/// A package as LuaRocks describes it.
#[derive(Debug, Default)]
pub struct LuarocksPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    /// Repository URL on search/outdated lines, rock tree path on
    /// list/show output.
    pub origin: Option<String>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for LuarocksPackage {
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
    fn parses_porcelain_list() {
        let stdout = "\
luasocket\t3.1.0-1\tinstalled\t/home/nick/.luarocks/lib/luarocks/rocks-5.4
luasocket\t3.0rc1-2\tinstalled\t/usr/lib/luarocks/rocks-5.4
penlight\t1.14.0-2\tinstalled\t/home/nick/.luarocks/lib/luarocks/rocks-5.4
";
        let packages = parse_list(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "luasocket");
        assert_eq!(packages[0].version.as_deref(), Some("3.1.0-1"));
        assert_eq!(
            packages[1].origin.as_deref(),
            Some("/usr/lib/luarocks/rocks-5.4")
        );
        assert_eq!(packages[2].state, InstallState::Installed);
    }

    #[test]
    fn search_keeps_the_first_line_per_rock() {
        let stdout = "\
luasocket\t3.1.0-1\tsrc\thttps://luarocks.org
luasocket\t3.1.0-1\trockspec\thttps://luarocks.org
luasocket\t3.0rc1-2\tsrc\thttps://luarocks.org
luasec\t1.3.2-1\tsrc\thttps://luarocks.org
";
        let packages = parse_search(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "luasocket");
        assert_eq!(packages[0].version.as_deref(), Some("3.1.0-1"));
        assert_eq!(packages[0].origin.as_deref(), Some("https://luarocks.org"));
        assert_eq!(packages[0].state, InstallState::Available);
        assert_eq!(packages[1].name, "luasec");
    }

    #[test]
    fn parses_porcelain_outdated() {
        let stdout = "luasocket\t3.0rc1-2\t3.1.0-1\thttps://luarocks.org\n";
        let packages = parse_outdated(stdout);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].version.as_deref(), Some("3.0rc1-2"));
        assert_eq!(packages[0].latest_version.as_deref(), Some("3.1.0-1"));
        assert_eq!(packages[0].state, InstallState::Upgradable);
    }

    #[test]
    fn parses_show_output() {
        let stdout = "\
luasocket 3.1.0-1 - Network support for the Lua language

LuaSocket is a Lua extension library composed of two parts: a C core that
provides support for the TCP and UDP transport layers, and a set of Lua
modules that add support for common protocols.

License:      MIT
Homepage:     http://lunarmodules.github.io/luasocket/
Installed in: /home/nick/.luarocks

Modules:
\tltn12 (/home/nick/.luarocks/share/lua/5.4/ltn12.lua)
\tmime (/home/nick/.luarocks/share/lua/5.4/mime.lua)

Depends on:
\tlua >= 5.1 (using 5.4-1)
";
        let package = parse_show(stdout).unwrap();
        assert_eq!(package.name, "luasocket");
        assert_eq!(package.version.as_deref(), Some("3.1.0-1"));
        assert_eq!(
            package.description.as_deref(),
            Some("Network support for the Lua language")
        );
        assert_eq!(package.license.as_deref(), Some("MIT"));
        assert_eq!(
            package.homepage.as_deref(),
            Some("http://lunarmodules.github.io/luasocket/")
        );
        assert_eq!(package.origin.as_deref(), Some("/home/nick/.luarocks"));
        assert_eq!(package.dependencies, Some(vec!["lua".to_string()]));
        assert_eq!(package.state, InstallState::Installed);
    }

    #[test]
    fn formats_version_pins_as_positional_args() {
        assert_eq!(
            spec(&PackageRequest::parse("luasocket@3.1.0-1")),
            vec!["luasocket".to_string(), "3.1.0-1".to_string()]
        );
        assert_eq!(
            spec(&PackageRequest::parse("luasocket")),
            vec!["luasocket".to_string()]
        );
    }
}
