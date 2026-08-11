//! TeX Live tlmgr backend for snowcone.
//!
//! TeX Live manager backend using `info --data` (CSV output) and
//! machine-readable updates. TeX Live has no separate metadata-refresh
//! verb: tlmgr re-reads the remote package database on every remote
//! operation, so REFRESH is not advertised. Mutations write into the
//! (usually root-owned) TeX Live tree, so they are elevated.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "tlmgr";
const PROGRAMS: &[&str] = &["tlmgr"];

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
        Cmd::new(&self.program).env("LC_ALL", "C")
    }
    async fn run(&self, cmd: Cmd, ctx: &OpContext) -> Result<()> {
        let out = match &ctx.events {
            Some(e) => cmd.capture(&self.elevator, Some(e)).await?,
            None => cmd.run_interactive(&self.elevator).await?,
        };
        out.require_success()?;
        Ok(())
    }
    fn mutation(&self, verb: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg(verb).elevated(true);
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        cmd
    }
    async fn info_data(&self, args: &[&str]) -> Result<Vec<TlmgrPackage>> {
        let out = self
            .cmd()
            .arg("info")
            .args(args)
            .args([
                "--data",
                "name,localrev,remoterev,cat-version,shortdesc,installed",
            ])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_info_data(&out.stdout))
    }
}

fn reject_pins(packages: &[PackageRequest]) -> Result<()> {
    if let Some(p) = packages.iter().find(|p| p.version.is_some()) {
        Err(Error::Other(format!(
            "{ID}: `{p}` cannot be pinned; TeX Live repositories expose revisions but tlmgr install accepts package names only"
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
        "TeX Live tlmgr"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "texlive"
    }

    /// No REFRESH: tlmgr has no metadata-refresh action (its action list
    /// stops at `update`, which mutates packages); the remote TLPDB is
    /// fetched anew by every remote operation.
    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
    }

    /// Mutations run through [`Manager::mutation`], which elevates: the
    /// TeX Live tree is normally root-owned. Reads never elevate.
    fn needs_elevation(&self, operation: Operation) -> bool {
        matches!(
            operation,
            Operation::Install | Operation::Remove | Operation::Upgrade
        )
    }

    async fn install(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        reject_pins(_packages)?;
        self.run(
            self.mutation("install", _ctx)
                .args(_packages.iter().map(|p| p.name.as_str())),
            _ctx,
        )
        .await
    }

    async fn remove(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        reject_pins(_packages)?;
        self.run(
            self.mutation("remove", _ctx)
                .args(_packages.iter().map(|p| p.name.as_str())),
            _ctx,
        )
        .await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.info_data(&["--only-installed"]).await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        self.info_data(&[name])
            .await?
            .into_iter()
            .find(|p| p.name == name)
            .map(|p| Box::new(p) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.into()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let out = self
            .cmd()
            .args(["search", "--global"])
            .arg(query)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        let names = parse_search_names(&out.stdout);
        if names.is_empty() {
            return Ok(Vec::new());
        }
        // One `info --data` call resolves every hit: `tlmgr info` accepts
        // multiple package names and prints one CSV row each.
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        Ok(boxed(self.info_data(&names).await?))
    }

    async fn upgrade(&self, _packages: &[PackageRequest], _ctx: &OpContext) -> Result<()> {
        reject_pins(_packages)?;
        let mut cmd = self.mutation("update", _ctx);
        if _packages.is_empty() {
            cmd = cmd.arg("--all");
        } else {
            cmd = cmd.args(_packages.iter().map(|p| p.name.as_str()));
        }
        self.run(cmd, _ctx).await
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let out = self
            .cmd()
            .args(["update", "--list", "--machine-readable"])
            .capture(&self.elevator, None)
            .await?;
        if !out.success() && out.stdout.trim().is_empty() {
            return Err(Error::Other(out.stderr));
        }
        Ok(boxed(parse_updates(&out.stdout)))
    }
}

fn boxed(v: Vec<TlmgrPackage>) -> Vec<Box<dyn Package>> {
    v.into_iter()
        .map(|p| Box::new(p) as Box<dyn Package>)
        .collect()
}
/// Split one line of `tlmgr info --data` output. tlmgr joins fields with
/// commas and wraps shortdesc/longdesc in double quotes, escaping embedded
/// quotes as `\"` - so quoted fields may contain commas.
fn split_csv(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            ',' if !quoted => fields.push(std::mem::take(&mut field)),
            '"' => quoted = !quoted,
            '\\' if quoted => {
                if let Some(escaped) = chars.next() {
                    field.push(escaped);
                }
            }
            _ => field.push(c),
        }
    }
    fields.push(field);
    fields
}
/// `info --data name,localrev,remoterev,cat-version,shortdesc,installed`:
/// one CSV row per package. Missing revisions are printed as `0` (not
/// installed / not available), cat-version is empty when the Catalogue has
/// none, installed is `1` or `0`.
fn parse_info_data(stdout: &str) -> Vec<TlmgrPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let f = split_csv(line);
            if f.len() < 6 {
                return None;
            }
            let name = f[0].trim();
            if name.is_empty() || name.contains(' ') {
                return None;
            }
            let local = f[1].trim().parse::<u64>().unwrap_or(0);
            let remote = f[2].trim().parse::<u64>().unwrap_or(0);
            let catalog = f[3].trim();
            let installed = f[5].trim() == "1";
            let state = if installed && local > 0 && remote > local {
                InstallState::Upgradable
            } else if installed {
                InstallState::Installed
            } else {
                InstallState::Available
            };
            let version = if !catalog.is_empty() {
                Some(catalog.into())
            } else if installed && local > 0 {
                Some(local.to_string())
            } else if remote > 0 {
                Some(remote.to_string())
            } else {
                None
            };
            Some(TlmgrPackage {
                name: name.into(),
                version,
                description: (!f[4].trim().is_empty()).then(|| f[4].trim().into()),
                state,
            })
        })
        .collect()
}
/// Plain `tlmgr search` prints one `name - shortdesc` line per package
/// hit (only `search --file` prints `name:` headers with indented file
/// lists). `tlmgr:` status lines carry no hits.
fn parse_search_names(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| {
            if line.starts_with(char::is_whitespace) || line.starts_with("tlmgr:") {
                return None;
            }
            let (name, _desc) = line.split_once(" - ")?;
            let name = name.trim();
            (!name.is_empty() && !name.contains(' ')).then(|| name.to_owned())
        })
        .collect()
}
fn parse_updates(stdout: &str) -> Vec<TlmgrPackage> {
    let mut body = false;
    stdout
        .lines()
        .filter_map(|line| {
            if line == "end-of-header" {
                body = true;
                return None;
            }
            if line == "end-of-updates" {
                body = false;
                return None;
            }
            if !body {
                return None;
            }
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 4 || !matches!(f[1], "u" | "a" | "i" | "I") {
                return None;
            }
            Some(TlmgrPackage {
                name: f[0].into(),
                version: (f[3] != "-").then(|| f[3].into()),
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
    fn splits_quoted_csv_fields() {
        // Real row captured from `tlmgr info --data ...` (TeX Live 2026):
        // quoted shortdesc containing commas.
        assert_eq!(
            split_csv(r#"a2ping,52964,52964,2.84p,"Advanced PS, PDF, EPS converter",1"#),
            [
                "a2ping",
                "52964",
                "52964",
                "2.84p",
                "Advanced PS, PDF, EPS converter",
                "1"
            ]
        );
        // Escaped quote inside a quoted field (tlmgr escapes `"` as `\"`).
        assert_eq!(
            split_csv(r#"x,1,2,,"a \"b\", c",0"#),
            ["x", "1", "2", "", r#"a "b", c"#, "0"]
        );
    }
    #[test]
    fn parses_data_rows() {
        // First two rows captured verbatim from `tlmgr info --only-installed
        // --data name,localrev,remoterev,cat-version,shortdesc,installed`;
        // the third row's revisions/installed flag are adjusted from the
        // first to exercise the upgradable and available branches (the
        // capture host had no pending updates), matching tlmgr's documented
        // `0` sentinels for missing revisions.
        let p = parse_info_data(
            "12many,15878,15878,0.3,\"Generalising mathematical index sets\",1\n\
             a2ping.aarch64-linux,46208,46208,,\"aarch64-linux files of a2ping\",1\n\
             12many,15878,16000,0.4,\"Generalising mathematical index sets\",1\n\
             latexmk,0,72097,4.86a,\"Fully automated LaTeX document generation\",0\n",
        );
        assert_eq!(p.len(), 4);
        assert_eq!(p[0].state, InstallState::Installed);
        assert_eq!(p[0].version.as_deref(), Some("0.3"));
        assert_eq!(p[1].version.as_deref(), Some("46208"));
        assert_eq!(p[2].state, InstallState::Upgradable);
        assert_eq!(p[3].state, InstallState::Available);
        assert_eq!(p[3].version.as_deref(), Some("4.86a"));
    }
    #[test]
    fn parses_machine_updates() {
        // Header captured from `tlmgr update --list --machine-readable`;
        // update rows follow tlmgr's machine_line format (tab-joined
        // pkg/flag/localrev/serverrev/size/runtime/esttot/tag/lcv/rcv) -
        // the capture host had no pending updates to record verbatim.
        let p = parse_updates(
            "location-url\t/usr/share\ntotal-bytes\t102400\nend-of-header\n\
             fontspec\tu\t70000\t71000\t102400\t-\t-\t-\t2.9a\t2.9g\n\
             old\td\t1\t-\t0\t-\t-\t-\t-\t-\nend-of-updates\n",
        );
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].version.as_deref(), Some("71000"));
    }
    #[test]
    fn parses_search_lines() {
        // Captured from `tlmgr search --global bibliography` (TeX Live 2026).
        let names = parse_search_names(
            "tlmgr: package repository /usr/share (not verified: unknown)\n\
             aichej - Bibliography style file for the AIChE Journal\n\
             bibarts - \"Arts\"-style bibliographical information\n",
        );
        assert_eq!(names, ["aichej", "bibarts"]);
    }
}

/// A package as tlmgr describes it.
#[derive(Debug)]
pub struct TlmgrPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for TlmgrPackage {
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
