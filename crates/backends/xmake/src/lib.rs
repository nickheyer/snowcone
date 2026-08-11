//! xmake / xrepo backend for snowcone.
//!
//! Supports standalone xrepo and xmake's equivalent `require` frontend.
//! Installed packages are enumerated with `xrepo scan` / `xmake require
//! --scan` (there is no `list` action; `require --list` shows a project's
//! declared dependencies, not what is installed).

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "xmake";
const PROGRAMS: &[&str] = &["xrepo", "xmake"];

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
                reason: format!("none of {PROGRAMS:?} found on PATH"),
            },
        }
    }

    fn create(&self, host: &HostInfo) -> Result<Box<dyn PackageManager>> {
        let program = PROGRAMS
            .iter()
            .find_map(|program| find_program(program))
            .ok_or_else(|| Error::Unavailable(ID.into()))?;
        let xrepo = program.file_stem().is_some_and(|name| name == "xrepo");
        Ok(Box::new(Manager {
            program,
            elevator: Elevator::detect(host),
            xrepo,
        }))
    }
}

struct Manager {
    program: PathBuf,
    elevator: Elevator,
    xrepo: bool,
}

impl Manager {
    /// `XMAKE_COLORTERM=nocolor` is xmake's own switch for disabling the
    /// `${color}` markup its cprint output otherwise carries.
    fn raw(&self) -> Cmd {
        Cmd::new(&self.program)
            .env("LC_ALL", "C")
            .env("XMAKE_COLORTERM", "nocolor")
    }

    fn cmd(&self) -> Cmd {
        if self.xrepo {
            self.raw()
        } else {
            self.raw().arg("require")
        }
    }

    async fn run(&self, cmd: Cmd, ctx: &OpContext) -> Result<()> {
        let out = match &ctx.events {
            Some(events) => cmd.capture(&self.elevator, Some(events)).await?,
            None => cmd.run_interactive(&self.elevator).await?,
        };
        out.require_success()?;
        Ok(())
    }

    fn no_dry_run(&self, ctx: &OpContext, operation: &str) -> Result<()> {
        if ctx.dry_run {
            Err(Error::Other(format!(
                "{ID}: {operation} has no dry-run mode"
            )))
        } else {
            Ok(())
        }
    }

    fn action(&self, xrepo_action: &str, xmake_flag: &str) -> Cmd {
        if self.xrepo {
            self.cmd().arg(xrepo_action)
        } else {
            self.cmd().arg(xmake_flag)
        }
    }

    /// The installed-package enumerator: `xrepo scan [packages]` (or
    /// `xmake require --scan`), which walks the package install directory.
    async fn scan(&self, package: Option<&str>) -> Result<Vec<XmakePackage>> {
        let mut cmd = if self.xrepo {
            self.cmd().arg("scan")
        } else {
            self.cmd().arg("--scan")
        };
        if let Some(package) = package {
            cmd = cmd.arg(package);
        }
        let out = cmd.capture(&self.elevator, None).await?.require_success()?;
        Ok(parse_scan(&out.stdout))
    }
}

fn spec(request: &PackageRequest) -> String {
    request.version.as_ref().map_or_else(
        || request.name.clone(),
        |version| format!("{} {version}", request.name),
    )
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "xmake / xrepo"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "xmake"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::REFRESH
            | Capabilities::UPGRADE
            | Capabilities::PIN_VERSION
    }

    /// Install is xrepo's `install` action; `xmake require` installs by
    /// default (it has no `--install` flag).
    async fn install(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        self.no_dry_run(_ctx, "install")?;
        let cmd = if self.xrepo {
            self.cmd().arg("install")
        } else {
            self.cmd()
        };
        self.run(cmd.args(_packages.iter().map(spec)), _ctx).await
    }

    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        self.no_dry_run(_ctx, "remove")?;
        self.run(
            self.action("remove", "--uninstall")
                .args(_packages.iter().map(|package| package.name.as_str())),
            _ctx,
        )
        .await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.scan(None).await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let out = self
            .action("info", "--info")
            .arg(name)
            .capture(&self.elevator, None)
            .await?;
        if !out.success() {
            return Err(Error::NotFound(name.into()));
        }
        let mut package = parse_info(&out.stdout, name);
        if self
            .scan(Some(name))
            .await?
            .into_iter()
            .any(|installed| installed.name == name)
        {
            package.state = InstallState::Installed;
        }
        Ok(Box::new(package))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let out = self
            .action("search", "--search")
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_search(&out.stdout)))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        self.no_dry_run(ctx, "refresh")?;
        let cmd = if self.xrepo {
            self.raw().arg("update-repo")
        } else {
            self.raw().args(["repo", "--update"])
        };
        self.run(cmd, ctx).await
    }

    /// `xmake require --upgrade` is the real upgrade switch (it bypasses
    /// the requires lock so newer versions resolve). xrepo has no upgrade
    /// action: reinstalling a named package resolves the latest version,
    /// which is the genuine per-package upgrade path, but there is no verb
    /// covering the whole installed set.
    async fn upgrade(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        self.no_dry_run(_ctx, "upgrade")?;
        let cmd = if self.xrepo {
            if _packages.is_empty() {
                return Err(Error::Other(format!(
                    "{ID}: xrepo has no upgrade-all verb; name the packages to upgrade"
                )));
            }
            self.cmd().arg("install")
        } else {
            self.cmd().arg("--upgrade")
        };
        self.run(cmd.args(_packages.iter().map(spec)), _ctx).await
    }
}

fn boxed(packages: Vec<XmakePackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

fn split_spec(value: &str) -> (String, Option<String>) {
    let mut fields = value.split_whitespace();
    let name = fields.next().unwrap_or(value).to_owned();
    let version = fields.next().map(str::to_owned);
    (name, version)
}

/// `name-version` scan headers: the version directory starts at the first
/// dash followed by a digit (`c-ares-1.34.5` → `c-ares` + `1.34.5`); a
/// branch version like `tbox-master` splits at the last dash instead.
fn split_scan_header(value: &str) -> (String, Option<String>) {
    for (idx, _) in value.match_indices('-') {
        if value[idx + 1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
        {
            return (value[..idx].to_owned(), Some(value[idx + 1..].to_owned()));
        }
    }
    match value.rsplit_once('-') {
        Some((name, version)) => (name.to_owned(), Some(version.to_owned())),
        None => (value.to_owned(), None),
    }
}

/// `xrepo scan`: a `name-version:` header per installed package version
/// (unindented), each followed by indented `-> <hash>: <plat>, <arch>` and
/// config detail lines; a `scanning packages ..` trace line leads.
fn parse_scan(stdout: &str) -> Vec<XmakePackage> {
    stdout
        .lines()
        .filter_map(|line| {
            if line.starts_with(char::is_whitespace) {
                return None;
            }
            let header = line.trim_end().strip_suffix(':')?;
            if header.is_empty() || header.contains(' ') {
                return None;
            }
            let (name, version) = split_scan_header(header);
            Some(XmakePackage {
                name,
                version,
                description: None,
                state: InstallState::Installed,
            })
        })
        .collect()
}

fn parse_search(stdout: &str) -> Vec<XmakePackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.strip_prefix("->")?.trim();
            let (left, description) = line.split_once(':').unwrap_or((line, ""));
            let (name, version) = split_spec(left);
            Some(XmakePackage {
                name,
                version,
                description: (!description.trim().is_empty())
                    .then(|| description.trim().to_owned()),
                state: InstallState::Available,
            })
        })
        .collect()
}

fn parse_info(stdout: &str, fallback_name: &str) -> XmakePackage {
    let mut name = fallback_name.to_owned();
    let mut version = None;
    let mut description = None;
    for line in stdout.lines() {
        let line = line.trim().trim_start_matches("->").trim();
        if let Some((key, value)) = line.split_once(':') {
            match key.trim().to_ascii_lowercase().as_str() {
                "name" => name = value.trim().to_owned(),
                "version" => version = Some(value.trim().to_owned()),
                "description" => description = Some(value.trim().to_owned()),
                _ => {}
            }
        }
    }
    XmakePackage {
        name,
        version,
        description,
        state: InstallState::Available,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_semantic_version_specs() {
        assert_eq!(spec(&PackageRequest::parse("zlib@1.2.x")), "zlib 1.2.x");
        assert_eq!(spec(&PackageRequest::parse("zlib")), "zlib");
    }

    #[test]
    fn parses_scan_headers() {
        // Shape from xmake's scan.lua: `name-version:` headers with
        // indented hash/config detail rows beneath each.
        let packages = parse_scan(
            "scanning packages ..\n\
             zlib-1.3.1:\n\
             \x20 -> 4b0f9d97a61c4289ad7e8ef65f9f0d48: linux, x86_64\n\
             \x20   -> {shared = false}\n\
             c-ares-1.34.5:\n\
             \x20 -> 8e26c817f4b6e4c69a63aaf00e0d0f1a: linux, x86_64, unused\n\
             tbox-master:\n\
             \x20 -> 1d3c8a05b2ef4f00a3b1a1c9e75d10bb: linux, x86_64\n",
        );
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "zlib");
        assert_eq!(packages[0].version.as_deref(), Some("1.3.1"));
        assert_eq!(packages[1].name, "c-ares");
        assert_eq!(packages[1].version.as_deref(), Some("1.34.5"));
        assert_eq!(packages[2].name, "tbox");
        assert_eq!(packages[2].version.as_deref(), Some("master"));
        assert_eq!(packages[0].state, InstallState::Installed);
    }

    #[test]
    fn parses_search_arrows() {
        let packages = parse_search(
            "zlib:\n  -> zlib 1.3.1: A compression library\n  -> minizip 1.3.1: Zip support\n",
        );
        assert_eq!(packages.len(), 2);
        assert_eq!(
            packages[0].description.as_deref(),
            Some("A compression library")
        );
    }
}

/// A package as xmake / xrepo describes it.
#[derive(Debug)]
pub struct XmakePackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for XmakePackage {
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
