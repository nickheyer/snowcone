//! Clojure tools.deps backend for snowcone.
//!
//! Covers the Clojure CLI's *tools* surface (`clojure -Ttools …`), the
//! closest thing tools.deps has to a package manager: per-user tools kept
//! under `~/.clojure/tools`. Requests name the procurer lib
//! (`io.github.seancorfield/deps-new`); the local tool name is its segment
//! after the last `/`, and remove accepts either spelling. Installs go
//! through the documented `install-latest`, which always resolves the
//! newest release - so pinned versions are refused. `-T` arguments are read
//! as EDN, which is why the remove argument is passed as a quoted string.
//! The CLI never prompts and nothing needs root.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "clojure";
const PROGRAMS: &[&str] = &["clojure", "clj"];

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

    /// Installed tools, from the `clojure -Ttools list` table.
    async fn tools(&self) -> Result<Vec<ClojurePackage>> {
        let output = self
            .query()
            .args(["-Ttools", "list"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_tools_list(&output.stdout))
    }
}

/// `install-latest` always resolves the lib's newest release; a pinned
/// version has no spelling here without `PIN_VERSION`.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but tools install only the latest release"
        ))),
        None => Ok(()),
    }
}

/// The local tool name a lib installs as: the segment after the last `/`,
/// or the name itself when it is already a bare tool name.
fn tool_alias(name: &str) -> &str {
    name.rsplit_once('/').map_or(name, |(_, artifact)| artifact)
}

/// A name as an EDN string literal - `-T` arguments are read as EDN, and a
/// quoted string is accepted wherever a tool name goes.
fn edn_string(name: &str) -> String {
    format!("\"{name}\"")
}

/// `clojure -Ttools list`: a `TOOL LIB TYPE VERSION` header, then one
/// aligned row per tool - the VERSION column holds the git tag for `:git`
/// procurers and the Maven version for `:mvn` ones.
fn parse_tools_list(stdout: &str) -> Vec<ClojurePackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let tool = parts.next()?;
            let lib = parts.next()?;
            if !parts.next()?.starts_with(':') {
                return None;
            }
            Some(ClojurePackage {
                name: tool.to_string(),
                version: parts.next().map(str::to_string),
                origin: Some(lib.to_string()),
                state: InstallState::Installed,
            })
        })
        .collect()
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Clojure tools.deps"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "jvm"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: install has no dry-run mode")));
        }
        if let Some(unqualified) = packages.iter().find(|package| !package.name.contains('/')) {
            return Err(Error::Other(format!(
                "{ID}: `{}` is not a qualified lib - tools install by coordinate, \
                 e.g. `io.github.seancorfield/deps-new`",
                unqualified.name
            )));
        }
        for package in packages {
            let cmd = self
                .cmd()
                .args(["-Ttools", "install-latest", ":lib"])
                .arg(&package.name)
                .arg(":as")
                .arg(tool_alias(&package.name));
            self.run(cmd, ctx).await?;
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: remove has no dry-run mode")));
        }
        for package in packages {
            let cmd = self
                .cmd()
                .args(["-Ttools", "remove", ":tool"])
                .arg(edn_string(tool_alias(&package.name)));
            self.run(cmd, ctx).await?;
        }
        Ok(())
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .tools()
            .await?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // Match the tool name, the alias a lib coordinate would install
        // as, or the lib coordinate itself.
        self.tools()
            .await?
            .into_iter()
            .find(|tool| {
                tool.name == name
                    || tool.name == tool_alias(name)
                    || tool.origin.as_deref() == Some(name)
            })
            .map(|tool| Box::new(tool) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }
}

/// A tool as `clojure -Ttools list` describes it.
#[derive(Debug, Default)]
pub struct ClojurePackage {
    /// Local tool name (`deps-new`), not the lib coordinate.
    pub name: String,
    /// Maven version or git tag, whichever the procurer uses.
    pub version: Option<String>,
    /// The lib coordinate the tool was installed from.
    pub origin: Option<String>,
    pub state: InstallState,
}

impl Package for ClojurePackage {
    fn manager(&self) -> &str {
        ID
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> Option<&str> {
        self.version.as_deref()
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
    fn parses_tools_list_table() {
        let stdout = "\
TOOL   LIB                              TYPE   VERSION
antq   com.github.liquidz/antq         :mvn   2.8.1173
new    io.github.seancorfield/deps-new :git   v0.5.2
tools  io.github.clojure/tools.tools   :git   v0.3.1
";
        let packages = parse_tools_list(stdout);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "antq");
        assert_eq!(packages[0].version.as_deref(), Some("2.8.1173"));
        assert_eq!(
            packages[0].origin.as_deref(),
            Some("com.github.liquidz/antq")
        );
        assert_eq!(packages[1].version.as_deref(), Some("v0.5.2"));
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn banner_lines_parse_to_nothing() {
        let stdout = "\
Cloning: https://github.com/clojure/tools.tools.git
Checking out: https://github.com/clojure/tools.tools.git at v0.3.1
TOOL LIB TYPE VERSION
";
        assert!(parse_tools_list(stdout).is_empty());
    }

    #[test]
    fn derives_tool_aliases() {
        assert_eq!(tool_alias("io.github.seancorfield/deps-new"), "deps-new");
        assert_eq!(tool_alias("com.github.liquidz/antq"), "antq");
        assert_eq!(tool_alias("deps-new"), "deps-new");
    }

    #[test]
    fn formats_edn_strings() {
        assert_eq!(edn_string("deps-new"), "\"deps-new\"");
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("io.github.foo/bar@1.0.0")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("io.github.foo/bar")]).is_ok());
    }
}
