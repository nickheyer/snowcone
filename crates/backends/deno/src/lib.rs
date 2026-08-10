//! Deno backend for snowcone.
//!
//! Drives `deno install -g` / `deno uninstall -g` (modern deno's global
//! script installs). Specifiers pass through verbatim - `npm:pkg`,
//! `jsr:@scope/pkg`, or a URL, resolved by deno's own rules - and a pinned
//! version is appended as `@version`. Deno has no listing verb: the honest
//! inventory is the shim files in `$DENO_INSTALL_ROOT/bin` (default
//! `~/.deno/bin`), read straight off the filesystem - names only, no
//! versions. Upgrade is a forced re-resolve (`-f --reload`) and therefore
//! needs the original specifier, so upgrading everything at once is
//! impossible and errors. No verb has a dry-run mode, and deno never
//! prompts during installs, so `assume_yes` has nothing to do.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "deno";
const PROGRAMS: &[&str] = &["deno"];

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

    fn no_dry_run(&self, operation: &str) -> Error {
        Error::Other(format!("{ID}: {operation} has no dry-run mode"))
    }

    /// The installed shim names, read from the global bin directory.
    fn shims(&self) -> Result<Vec<String>> {
        let dir = bin_dir(
            std::env::var("DENO_INSTALL_ROOT").ok().as_deref(),
            std::env::var("HOME").ok().as_deref(),
        )
        .ok_or_else(|| {
            Error::Other(format!(
                "{ID}: cannot locate the install root (neither DENO_INSTALL_ROOT nor HOME is set)"
            ))
        })?;
        list_shims(&dir)
    }
}

/// `name@version` when the request pins one, the bare specifier otherwise.
fn spec(request: &PackageRequest) -> String {
    match &request.version {
        Some(version) => format!("{}@{version}", request.name),
        None => request.name.clone(),
    }
}

/// The global shim directory: `$DENO_INSTALL_ROOT/bin` when the variable is
/// set, `$HOME/.deno/bin` otherwise - deno's own precedence.
fn bin_dir(install_root: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(root) = install_root.filter(|root| !root.is_empty()) {
        return Some(Path::new(root).join("bin"));
    }
    home.filter(|home| !home.is_empty())
        .map(|home| Path::new(home).join(".deno").join("bin"))
}

/// The installed shims in one bin directory: every executable regular file,
/// minus the `deno` runtime binary the official installer parks alongside
/// them. A missing directory means nothing was ever installed.
fn list_shims(dir: &Path) -> Result<Vec<String>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name == "deno" {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(entry.path()) else {
            continue;
        };
        if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

fn shim_package(name: String) -> Box<dyn Package> {
    Box::new(DenoPackage {
        name,
        state: InstallState::Installed,
    })
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Deno"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Language
    }

    fn database_id(&self) -> &'static str {
        "node"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE | Capabilities::UPGRADE | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        // One specifier per invocation: extra positionals to `deno install`
        // become arguments baked into the shim, not more packages.
        for package in packages {
            self.run(self.cmd().args(["install", "-g"]).arg(spec(package)), ctx)
                .await?;
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        for package in packages {
            self.run(
                self.cmd().args(["uninstall", "-g"]).arg(&package.name),
                ctx,
            )
            .await?;
        }
        Ok(())
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self.shims()?.into_iter().map(shim_package).collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        self.shims()?
            .into_iter()
            .find(|shim| shim == name)
            .map(shim_package)
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        if packages.is_empty() {
            return Err(Error::Other(format!(
                "{ID}: upgrade needs explicit specifiers - installed shims do not record where they came from"
            )));
        }
        for package in packages {
            self.run(
                self.cmd()
                    .args(["install", "-g", "-f", "--reload"])
                    .arg(spec(package)),
                ctx,
            )
            .await?;
        }
        Ok(())
    }
}

/// A package as deno describes it: a shim name in the global bin directory.
#[derive(Debug)]
pub struct DenoPackage {
    pub name: String,
    pub state: InstallState,
}

impl Package for DenoPackage {
    fn manager(&self) -> &str {
        ID
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> Option<&str> {
        None
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_bin_dir_precedence() {
        assert_eq!(
            bin_dir(Some("/opt/deno"), Some("/home/nick")),
            Some(PathBuf::from("/opt/deno/bin"))
        );
        assert_eq!(
            bin_dir(None, Some("/home/nick")),
            Some(PathBuf::from("/home/nick/.deno/bin"))
        );
        assert_eq!(
            bin_dir(Some(""), Some("/home/nick")),
            Some(PathBuf::from("/home/nick/.deno/bin"))
        );
        assert_eq!(bin_dir(None, None), None);
    }

    #[test]
    fn lists_executable_shims_only() {
        let dir = std::env::temp_dir().join(format!("snowcone-deno-shims-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let executable = std::fs::Permissions::from_mode(0o755);
        let plain = std::fs::Permissions::from_mode(0o644);
        for (name, perms) in [
            ("cowsay", &executable),
            ("vite", &executable),
            ("deno", &executable),
            ("notes.txt", &plain),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, perms.clone()).unwrap();
        }
        std::fs::create_dir(dir.join("subdir")).unwrap();
        let names = list_shims(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(names, vec!["cowsay".to_string(), "vite".to_string()]);
    }

    #[test]
    fn missing_bin_dir_lists_nothing() {
        let dir = std::env::temp_dir().join(format!(
            "snowcone-deno-missing-{}/does-not-exist",
            std::process::id()
        ));
        assert!(list_shims(&dir).unwrap().is_empty());
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("npm:cowsay@1.6.0")),
            "npm:cowsay@1.6.0"
        );
        assert_eq!(
            spec(&PackageRequest {
                name: "jsr:@std/http".to_string(),
                version: Some("1.0.0".to_string()),
            }),
            "jsr:@std/http@1.0.0"
        );
        assert_eq!(spec(&PackageRequest::parse("npm:cowsay")), "npm:cowsay");
    }
}
