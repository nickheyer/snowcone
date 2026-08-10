//! Quicklisp backend for snowcone.
//!
//! Quicklisp is an in-process Common Lisp client rather than an executable,
//! so this backend drives its public API through a non-interactive SBCL. The
//! package unit is a Quicklisp system; its release prefix is the immutable
//! snapshot identifier Quicklisp exposes in place of a semantic version.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "quicklisp";
const PROGRAMS: &[&str] = &["sbcl"];
const RECORD: &str = "SNOWCONE";

pub fn factory() -> Box<dyn BackendFactory> {
    Box::new(Factory)
}

struct Factory;

impl BackendFactory for Factory {
    fn id(&self) -> &'static str {
        ID
    }

    fn detect(&self, _host: &HostInfo) -> Detection {
        let Some(program) = PROGRAMS.iter().find_map(|program| find_program(program)) else {
            return Detection::Unavailable {
                reason: format!("`{}` not found on PATH", PROGRAMS[0]),
            };
        };
        if quicklisp_setup().is_none() {
            return Detection::Unavailable {
                reason: "Quicklisp setup.lisp not found (set QUICKLISP_SETUP or QUICKLISP_HOME)"
                    .into(),
            };
        }
        Detection::Available { program }
    }

    fn create(&self, host: &HostInfo) -> Result<Box<dyn PackageManager>> {
        let program = find_program(PROGRAMS[0]).ok_or_else(|| Error::Unavailable(ID.into()))?;
        let setup = quicklisp_setup().ok_or_else(|| {
            Error::Unavailable(format!(
                "{ID}: setup.lisp not found (set QUICKLISP_SETUP or QUICKLISP_HOME)"
            ))
        })?;
        Ok(Box::new(Manager {
            program,
            setup,
            elevator: Elevator::detect(host),
        }))
    }
}

fn quicklisp_setup() -> Option<PathBuf> {
    if let Some(setup) = std::env::var_os("QUICKLISP_SETUP") {
        let setup = PathBuf::from(setup);
        if setup.is_file() {
            return Some(setup);
        }
    }
    if let Some(home) = std::env::var_os("QUICKLISP_HOME") {
        let setup = PathBuf::from(home).join("setup.lisp");
        if setup.is_file() {
            return Some(setup);
        }
    }
    let home = std::env::var_os("HOME")?;
    let home = Path::new(&home);
    [
        home.join("quicklisp/setup.lisp"),
        home.join(".quicklisp/setup.lisp"),
        home.join(".roswell/lisp/quicklisp/setup.lisp"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

struct Manager {
    program: PathBuf,
    setup: PathBuf,
    elevator: Elevator,
}

impl Manager {
    fn eval(&self, expression: impl Into<String>) -> Cmd {
        Cmd::new(&self.program)
            .args([
                "--noinform",
                "--no-userinit",
                "--disable-debugger",
                "--non-interactive",
                "--load",
            ])
            .arg(&self.setup)
            .arg("--eval")
            .arg(expression.into())
            .env("LC_ALL", "C")
    }

    async fn run(&self, cmd: Cmd, ctx: &OpContext) -> Result<()> {
        let output = match &ctx.events {
            Some(events) => cmd.capture(&self.elevator, Some(events)).await?,
            None => cmd.run_interactive(&self.elevator).await?,
        };
        output.require_success()?;
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
}

fn lisp_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '\\' | '"' => {
                escaped.push('\\');
                escaped.push(character);
            }
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            character => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
}

fn lisp_list(packages: &[PackageRequest]) -> String {
    format!(
        "(list {})",
        packages
            .iter()
            .map(|package| lisp_string(&package.name))
            .collect::<Vec<_>>()
            .join(" ")
    )
}

fn reject_versions(packages: &[PackageRequest]) -> Result<()> {
    if let Some(package) = packages.iter().find(|package| package.version.is_some()) {
        Err(Error::Other(format!(
            "{ID}: `{package}` cannot be pinned; versions are selected by the active dist"
        )))
    } else {
        Ok(())
    }
}

fn record_form(system: &str, state: &str) -> String {
    format!(
        r#"(format t "{RECORD}~C~A~C~A~C~A~C~A~%" #\Tab (ql-dist:name {system}) #\Tab (ql-dist:short-description (ql-dist:release {system})) #\Tab (ql-dist:project-name (ql-dist:release {system})) #\Tab {state})"#
    )
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Quicklisp"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "quicklisp"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::SEARCH | Capabilities::UPGRADE
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_versions(packages)?;
        self.no_dry_run(ctx, "install")?;
        let expression = format!(
            "(ql:quickload {} :prompt nil :verbose t)",
            lisp_list(packages)
        );
        self.run(self.eval(expression), ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_versions(packages)?;
        self.no_dry_run(ctx, "remove")?;
        let expression = format!(
            "(progn {})",
            packages
                .iter()
                .map(|package| format!("(ql:uninstall {})", lisp_string(&package.name)))
                .collect::<Vec<_>>()
                .join(" ")
        );
        self.run(self.eval(expression), ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        let expression = format!(
            "(dolist (system (ql-dist:installed-systems t)) {})",
            record_form("system", "\"installed\"")
        );
        let output = self
            .eval(expression)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_records(&output.stdout)))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let record = record_form(
            "system",
            "(if (ql-dist:installedp system) \"installed\" \"available\")",
        );
        let expression = format!(
            "(let ((system (ql-dist:find-system {}))) (unless system (error \"Quicklisp system not found\")) {record})",
            lisp_string(name)
        );
        let output = self.eval(expression).capture(&self.elevator, None).await?;
        if !output.success() {
            return Err(Error::NotFound(name.into()));
        }
        parse_records(&output.stdout)
            .into_iter()
            .next()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.into()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        let record = record_form(
            "system",
            "(if (ql-dist:installedp system) \"installed\" \"available\")",
        );
        let expression = format!(
            "(dolist (system (ql:system-apropos-list {})) {record})",
            lisp_string(query)
        );
        let output = self
            .eval(expression)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(boxed(parse_records(&output.stdout)))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_versions(packages)?;
        self.no_dry_run(ctx, "upgrade")?;
        // Quicklisp updates subscribed dists as atomic snapshots and
        // reinstalls every installed release affected by the snapshot.
        let expression = if packages.is_empty() {
            "(ql:update-all-dists :prompt nil)".into()
        } else {
            format!(
                "(progn (ql:update-all-dists :prompt nil) (ql:quickload {} :prompt nil :verbose t))",
                lisp_list(packages)
            )
        };
        self.run(self.eval(expression), ctx).await
    }
}

fn parse_records(stdout: &str) -> Vec<QuicklispPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            if fields.next()? != RECORD {
                return None;
            }
            let name = fields.next()?.to_string();
            let version = fields.next().and_then(|value| {
                let value = value.trim();
                (!value.is_empty()).then(|| value.to_string())
            });
            let description = fields.next().and_then(|value| {
                let value = value.trim();
                (!value.is_empty()).then(|| value.to_string())
            });
            let state = match fields.next()?.trim() {
                "installed" => InstallState::Installed,
                "outdated" => InstallState::Upgradable,
                _ => InstallState::Available,
            };
            Some(QuicklispPackage {
                name,
                version,
                description,
                state,
            })
        })
        .collect()
}

fn boxed(packages: Vec<QuicklispPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// A Quicklisp system and the release snapshot that provides it.
#[derive(Debug)]
pub struct QuicklispPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub state: InstallState,
}

impl Package for QuicklispPackage {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_lisp_strings() {
        assert_eq!(lisp_string("a\\b\"c"), r#""a\\b\"c""#);
        assert_eq!(lisp_string("a\nb"), r#""a\nb""#);
    }

    #[test]
    fn builds_system_lists_without_code_injection() {
        let packages = [
            PackageRequest::parse("alexandria"),
            PackageRequest::parse("bad\") (error \"x"),
        ];
        assert_eq!(
            lisp_list(&packages),
            r#"(list "alexandria" "bad\") (error \"x")"#
        );
    }

    #[test]
    fn parses_only_tagged_records() {
        let output = "Quicklisp setup loaded\nSNOWCONE\talexandria\talexandria-20250503-git\tAlexandria\tinstalled\nSNOWCONE\tbordeaux-threads\tbordeaux-threads-v0.8.8\tBordeaux Threads\tavailable\n";
        let packages = parse_records(output);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "alexandria");
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[1].state, InstallState::Available);
    }

    #[test]
    fn rejects_dist_version_pins() {
        assert!(reject_versions(&[PackageRequest::parse("alexandria@1.0")]).is_err());
    }
}
