//! Yarn backend for snowcone.
//!
//! Drives yarn classic's (1.x) global surface: `yarn global
//! add/remove/list/upgrade`. Yarn Berry (2+) removed the `global` verb
//! entirely, so on Berry every operation fails with yarn's own usage error
//! carried in the `CommandFailed` - no version probe, the message is
//! self-explanatory. Globals are per-user, so nothing elevates. `yarn info`
//! answers in NDJSON (one `{type, data}` object per line) and can exit 0
//! even when the lookup failed, so the parser hunts for the `inspect` line
//! instead of trusting the exit code. Classic has no dry-run flag on any of
//! these verbs; `assume_yes` maps to `--non-interactive`.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "yarn";
const PROGRAMS: &[&str] = &["yarn"];

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

    /// Shared shape of mutating global commands: `yarn global <verb>`, with
    /// `--non-interactive` under `assume_yes`.
    fn mutation(&self, subcommand: &str, ctx: &OpContext) -> Cmd {
        let mut cmd = self.cmd().arg("global").arg(subcommand);
        if ctx.assume_yes {
            cmd = cmd.arg("--non-interactive");
        }
        cmd
    }

    fn no_dry_run(&self, operation: &str) -> Error {
        Error::Other(format!("{ID}: {operation} has no dry-run mode"))
    }

    /// Globally installed packages, from `yarn global list`.
    async fn global_list(&self) -> Result<Vec<YarnPackage>> {
        let output = self
            .query()
            .args(["global", "list"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        Ok(parse_global_list(&output.stdout))
    }
}

/// `name@version` when the request pins one, bare name otherwise.
fn spec(request: &PackageRequest) -> String {
    match &request.version {
        Some(version) => format!("{}@{version}", request.name),
        None => request.name.clone(),
    }
}

/// Split `name@version`; a leading `@` (npm scopes) belongs to the name.
fn split_spec(spec: &str) -> (String, Option<String>) {
    match spec.char_indices().skip(1).find(|&(_, c)| c == '@') {
        Some((at, _)) => (
            spec[..at].to_string(),
            Some(spec[at + 1..].to_string()).filter(|version| !version.is_empty()),
        ),
        None => (spec.to_string(), None),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Yarn"
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
        let cmd = self.mutation("add", ctx).args(packages.iter().map(spec));
        self.run(cmd, ctx).await
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        let cmd = self
            .mutation("remove", ctx)
            .args(packages.iter().map(|package| package.name.as_str()));
        self.run(cmd, ctx).await
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(self
            .global_list()
            .await?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let output = self
            .cmd()
            .arg("info")
            .arg(name)
            .arg("--json")
            .capture(&self.elevator, None)
            .await?;
        if !output.success() {
            return Err(Error::NotFound(name.to_string()));
        }
        let mut package =
            parse_info(&output.stdout).ok_or_else(|| Error::NotFound(name.to_string()))?;
        // The registry view says nothing about the local install.
        if let Some(installed) = self
            .global_list()
            .await?
            .into_iter()
            .find(|installed| installed.name == package.name)
        {
            package.state = InstallState::Installed;
            if installed.version.is_some() && installed.version != package.version {
                package.latest_version = package.version.take();
                package.version = installed.version;
            }
        }
        Ok(Box::new(package))
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("upgrade"));
        }
        let cmd = if packages.is_empty() {
            self.mutation("upgrade", ctx)
        } else {
            // `global add name@latest` upgrades past whatever semver range
            // the original install recorded; `global upgrade` would respect
            // it.
            self.mutation("add", ctx)
                .args(packages.iter().map(|package| match &package.version {
                    Some(version) => format!("{}@{version}", package.name),
                    None => format!("{}@latest", package.name),
                }))
        };
        self.run(cmd, ctx).await
    }
}

/// `yarn global list`: one `info "name@version" has binaries:` line per
/// package, each followed by its indented binary names (ignored here).
fn parse_global_list(stdout: &str) -> Vec<YarnPackage> {
    stdout
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("info \"")?;
            let (spec, _) = rest.split_once('"')?;
            let (name, version) = split_spec(spec);
            Some(YarnPackage {
                name,
                version,
                state: InstallState::Installed,
                ..Default::default()
            })
        })
        .collect()
}

/// `yarn info --json`: NDJSON lines; the `inspect` line's `data` is the
/// registry manifest of the latest version (`license` is a string on modern
/// packages and an object on ancient ones). Yarn can exit 0 with only an
/// `error` line, so a missing `inspect` line means "not found".
fn parse_info(stdout: &str) -> Option<YarnPackage> {
    let data = stdout.lines().find_map(|line| {
        let json: Value = serde_json::from_str(line.trim()).ok()?;
        (json["type"].as_str()? == "inspect").then_some(json["data"].clone())
    })?;
    let name = data["name"].as_str()?;
    let license = data["license"]
        .as_str()
        .map(str::to_string)
        .or_else(|| data["license"]["type"].as_str().map(str::to_string));
    Some(YarnPackage {
        name: name.to_string(),
        version: data["version"]
            .as_str()
            .or_else(|| data["dist-tags"]["latest"].as_str())
            .map(str::to_string),
        description: data["description"].as_str().map(str::to_string),
        homepage: data["homepage"].as_str().map(str::to_string),
        license,
        dependencies: data["dependencies"]
            .as_object()
            .map(|map| map.keys().cloned().collect()),
        state: InstallState::Available,
        ..Default::default()
    })
}

/// A package as yarn describes it.
#[derive(Debug, Default)]
pub struct YarnPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for YarnPackage {
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
    fn parses_global_list_lines() {
        let stdout = "\
yarn global v1.22.22
info \"create-react-app@5.0.1\" has binaries:
   - create-react-app
info \"@angular/cli@17.3.8\" has binaries:
   - ng
Done in 0.11s.
";
        let packages = parse_global_list(stdout);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "create-react-app");
        assert_eq!(packages[0].version.as_deref(), Some("5.0.1"));
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[1].name, "@angular/cli");
        assert_eq!(packages[1].version.as_deref(), Some("17.3.8"));
    }

    #[test]
    fn parses_info_inspect_line() {
        let stdout = r#"{"type":"warning","data":"package.json: No license field"}
{"type":"inspect","data":{"name":"typescript","version":"5.5.3","description":"TypeScript is a language for application scale JavaScript development","license":"Apache-2.0","homepage":"https://www.typescriptlang.org/","dist-tags":{"latest":"5.5.3"},"dependencies":{"minimist":"^1.2.6"}}}
"#;
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.name, "typescript");
        assert_eq!(package.version.as_deref(), Some("5.5.3"));
        assert_eq!(package.license.as_deref(), Some("Apache-2.0"));
        assert_eq!(package.dependencies, Some(vec!["minimist".to_string()]));
        assert_eq!(package.state, InstallState::Available);
    }

    #[test]
    fn parses_info_with_object_license_and_dist_tag_version() {
        let stdout = r#"{"type":"inspect","data":{"name":"left-pad","dist-tags":{"latest":"1.3.0"},"license":{"type":"WTFPL"}}}"#;
        let package = parse_info(stdout).unwrap();
        assert_eq!(package.version.as_deref(), Some("1.3.0"));
        assert_eq!(package.license.as_deref(), Some("WTFPL"));
    }

    #[test]
    fn info_without_inspect_line_is_not_found() {
        let stdout = r#"{"type":"error","data":"Received invalid response from npm."}"#;
        assert!(parse_info(stdout).is_none());
    }

    #[test]
    fn splits_specs_keeping_scopes() {
        assert_eq!(
            split_spec("@types/node@22.0.0"),
            ("@types/node".to_string(), Some("22.0.0".to_string()))
        );
        assert_eq!(split_spec("ripgrep"), ("ripgrep".to_string(), None));
    }

    #[test]
    fn formats_version_pins() {
        assert_eq!(
            spec(&PackageRequest::parse("@types/node@22.0.0")),
            "@types/node@22.0.0"
        );
        assert_eq!(spec(&PackageRequest::parse("typescript")), "typescript");
    }
}
