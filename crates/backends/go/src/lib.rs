//! Go backend for snowcone, covering `go install`-managed binaries.
//!
//! Go has no package database of its own: `go install pkg@version` drops a
//! binary into GOBIN and that directory *is* the install state. Listing
//! reads it back with `go version -m`, which reports the embedded module
//! metadata of every binary in a directory; remove deletes the binary,
//! because that is the accepted uninstall story in the Go ecosystem.
//! Install targets must be full module paths (`github.com/junegunn/fzf`),
//! exactly as `go install` itself demands.

use std::path::PathBuf;

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, ProgressEvent, Result,
    find_program,
};

const ID: &str = "go";
const PROGRAMS: &[&str] = &["go"];

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

    /// Where `go install` puts binaries: GOBIN when set, else GOPATH/bin.
    async fn gobin(&self) -> Result<PathBuf> {
        let output = self
            .cmd()
            .args(["env", "GOBIN", "GOPATH"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        let mut lines = output.stdout.lines();
        let gobin = lines.next().unwrap_or("").trim();
        if !gobin.is_empty() {
            return Ok(PathBuf::from(gobin));
        }
        let gopath = lines.next().unwrap_or("").trim();
        let first = gopath.split(':').next().unwrap_or("");
        if first.is_empty() {
            return Err(Error::Other(format!(
                "{ID}: neither GOBIN nor GOPATH is set"
            )));
        }
        Ok(PathBuf::from(first).join("bin"))
    }

    async fn installed(&self) -> Result<Vec<GoPackage>> {
        let gobin = self.gobin().await?;
        if !gobin.is_dir() {
            return Ok(Vec::new());
        }
        // `go version -m <dir>` reports every Go binary in the directory;
        // non-Go files only produce stderr noise.
        let output = self
            .cmd()
            .args(["version", "-m"])
            .arg(&gobin)
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_version_m(&output.stdout))
    }
}

/// `path@vX.Y.Z` / `path@latest` as `go install` wants it; bare numeric
/// versions get the `v` Go requires.
fn spec(request: &PackageRequest) -> String {
    match &request.version {
        Some(version) if version.starts_with(|c: char| c.is_ascii_digit()) => {
            format!("{}@v{version}", request.name)
        }
        Some(version) => format!("{}@{version}", request.name),
        None => format!("{}@latest", request.name),
    }
}

/// The binary a module path produces: its last segment, skipping a `/vN`
/// major-version suffix (`…/bar/v2` builds `bar`).
fn binary_name(module: &str) -> &str {
    let mut segments = module.trim_end_matches('/').rsplit('/');
    let last = segments.next().unwrap_or(module);
    let is_major =
        last.len() > 1 && last.starts_with('v') && last[1..].chars().all(|c| c.is_ascii_digit());
    if is_major {
        segments.next().unwrap_or(last)
    } else {
        last
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Go"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "go"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::UPGRADE | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let mut cmd = self.cmd().arg("install");
        if ctx.dry_run {
            // `-n` prints the build steps without running them.
            cmd = cmd.arg("-n");
        }
        self.run(cmd.args(packages.iter().map(spec)), ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let gobin = self.gobin().await?;
        for package in packages {
            let path = gobin.join(binary_name(&package.name));
            if !path.is_file() {
                return Err(Error::NotFound(package.name.clone()));
            }
            let message = if ctx.dry_run {
                format!("would remove {}", path.display())
            } else {
                std::fs::remove_file(&path)?;
                format!("removed {}", path.display())
            };
            match &ctx.events {
                Some(_) => ctx.emit(ProgressEvent::Status(message)),
                None => println!("{message}"),
            }
        }
        Ok(())
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .installed()
            .await?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        // Match a full module path or a bare binary name.
        self.installed()
            .await?
            .into_iter()
            .find(|package| package.name == name || binary_name(&package.name) == name)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        let targets: Vec<String> = if packages.is_empty() {
            // Reinstalling every tracked binary at @latest is Go's upgrade.
            self.installed()
                .await?
                .into_iter()
                .map(|package| format!("{}@latest", package.name))
                .collect()
        } else {
            // A pinned request upgrades (or moves) to exactly that version;
            // an unpinned one goes to @latest.
            packages.iter().map(spec).collect()
        };
        if targets.is_empty() {
            return Ok(());
        }
        let mut cmd = self.cmd().arg("install");
        if ctx.dry_run {
            cmd = cmd.arg("-n");
        }
        self.run(cmd.args(targets), ctx).await
    }
}

/// `go version -m <dir>`: one unindented `"/path/to/bin: go1.x"` line per
/// binary, followed by tab-indented `path`/`mod`/`dep` records. The `path`
/// value is the installable main package; `mod` carries the version.
fn parse_version_m(stdout: &str) -> Vec<GoPackage> {
    let mut packages = Vec::new();
    let mut current: Option<GoPackage> = None;
    for line in stdout.lines() {
        if !line.starts_with('\t') {
            packages.extend(current.take().filter(|p| !p.name.is_empty()));
            current = Some(GoPackage {
                state: InstallState::Installed,
                ..Default::default()
            });
            continue;
        }
        let Some(package) = current.as_mut() else {
            continue;
        };
        let mut fields = line.split('\t').skip(1);
        match (fields.next(), fields.next(), fields.next()) {
            (Some("path"), Some(path), _) => package.name = path.to_string(),
            (Some("mod"), Some(_), Some(version)) => {
                package.version = Some(version.to_string());
            }
            _ => {}
        }
    }
    packages.extend(current.take().filter(|p| !p.name.is_empty()));
    packages
}

/// A binary as `go version -m` describes it.
#[derive(Debug, Default)]
pub struct GoPackage {
    /// Main package import path (`github.com/junegunn/fzf`).
    pub name: String,
    pub version: Option<String>,
    pub state: InstallState,
}

impl Package for GoPackage {
    fn manager(&self) -> &str {
        ID
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_m_output() {
        let stdout = "\
/home/nick/go/bin/fzf: go1.22.1
\tpath\tgithub.com/junegunn/fzf
\tmod\tgithub.com/junegunn/fzf\tv0.46.1\th1:aaa=
\tdep\tgithub.com/rivo/uniseg\tv0.4.7\th1:bbb=
/home/nick/go/bin/gopls: go1.22.1
\tpath\tgolang.org/x/tools/gopls
\tmod\tgolang.org/x/tools/gopls\tv0.15.3\th1:ccc=
";
        let packages = parse_version_m(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "github.com/junegunn/fzf");
        assert_eq!(packages[0].version.as_deref(), Some("v0.46.1"));
        assert_eq!(packages[1].name, "golang.org/x/tools/gopls");
        assert_eq!(packages[1].state, InstallState::Installed);
    }

    #[test]
    fn skips_binaries_without_module_info() {
        let stdout = "/home/nick/go/bin/rustup: could not read Go build info\n";
        assert!(parse_version_m(stdout).is_empty());
    }

    #[test]
    fn formats_install_specs() {
        assert_eq!(
            spec(&PackageRequest::parse("github.com/junegunn/fzf@0.46.1")),
            "github.com/junegunn/fzf@v0.46.1"
        );
        assert_eq!(
            spec(&PackageRequest::parse("github.com/junegunn/fzf@latest")),
            "github.com/junegunn/fzf@latest"
        );
        assert_eq!(
            spec(&PackageRequest::parse("github.com/junegunn/fzf")),
            "github.com/junegunn/fzf@latest"
        );
    }

    #[test]
    fn binary_names_skip_major_version_suffixes() {
        assert_eq!(binary_name("github.com/foo/bar"), "bar");
        assert_eq!(binary_name("github.com/foo/bar/v2"), "bar");
        assert_eq!(binary_name("fzf"), "fzf");
    }
}
