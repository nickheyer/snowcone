//! Helm backend for snowcone.
//!
//! Installed packages are Kubernetes releases, while search and install
//! operate on charts. A bare install target is a chart and uses Helm's
//! generated release name. `release=chart` selects an explicit release;
//! `namespace/release=chart` additionally selects its namespace. Upgrade
//! can infer a chart from release metadata, or accept the same explicit
//! syntax when repository names are ambiguous.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Package, PackageManager, PackageRequest, Result, find_program,
};

const ID: &str = "helm";
const PROGRAMS: &[&str] = &["helm"];

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
        Cmd::new(&self.program)
    }

    fn query(&self) -> Cmd {
        self.cmd().env("LC_ALL", "C")
    }

    async fn run(&self, cmd: Cmd, ctx: &OpContext) -> Result<()> {
        let output = match &ctx.events {
            Some(events) => cmd.capture(&self.elevator, Some(events)).await?,
            None => cmd.run_interactive(&self.elevator).await?,
        };
        output.require_success()?;
        Ok(())
    }

    async fn installed(&self) -> Result<Vec<HelmPackage>> {
        const PAGE: usize = 256;
        let mut packages = Vec::new();
        let mut offset = 0;
        loop {
            let output = self
                .query()
                .args([
                    "list",
                    "--all-namespaces",
                    "--output=json",
                    &format!("--max={PAGE}"),
                    &format!("--offset={offset}"),
                ])
                .capture(&self.elevator, None)
                .await?
                .require_success()?;
            let page = parse_release_json(&output.stdout)?;
            let count = page.len();
            packages.extend(page);
            if count < PAGE {
                break;
            }
            offset += count;
        }
        Ok(packages)
    }

    async fn search_repo(&self, query: &str) -> Result<Vec<HelmPackage>> {
        let output = self
            .query()
            .args(["search", "repo", query, "--output=json"])
            .capture(&self.elevator, None)
            .await?
            .require_success()?;
        parse_search_json(&output.stdout)
    }

    async fn release(&self, locator: &str) -> Result<HelmPackage> {
        let (namespace, release) = release_locator(locator);
        let matches: Vec<_> = self
            .installed()
            .await?
            .into_iter()
            .filter(|package| {
                package.name == release
                    && namespace.is_none_or(|namespace| package.namespace == namespace)
            })
            .collect();
        match matches.len() {
            0 => Err(Error::NotFound(locator.into())),
            1 => Ok(matches.into_iter().next().unwrap()),
            _ => Err(Error::Other(format!(
                "{ID}: release `{locator}` exists in multiple namespaces; use namespace/release"
            ))),
        }
    }

    async fn chart_for(&self, release: &HelmPackage) -> Result<String> {
        let chart = release.chart.as_deref().ok_or_else(|| Error::Parse {
            what: format!("{ID} release metadata"),
            detail: format!("release `{}` has no chart name", release.name),
        })?;
        self.search_repo(chart)
            .await?
            .into_iter()
            .find(|package| {
                package
                    .name
                    .rsplit_once('/')
                    .map_or(package.name.as_str(), |(_, name)| name)
                    == chart
            })
            .map(|package| package.name)
            .ok_or_else(|| {
                Error::Other(format!(
                    "{ID}: cannot map installed chart `{chart}` to a configured repository; use release=repo/chart"
                ))
            })
    }

    async fn upgrade_one(&self, package: &PackageRequest, ctx: &OpContext) -> Result<()> {
        let (locator, explicit_chart) = package
            .name
            .split_once('=')
            .map_or((package.name.as_str(), None), |(release, chart)| {
                (release, Some(chart))
            });
        let installed = self.release(locator).await?;
        let chart = match explicit_chart {
            Some(chart) => chart.to_string(),
            None => self.chart_for(&installed).await?,
        };
        let mut cmd = self
            .cmd()
            .args(["upgrade", installed.name.as_str(), chart.as_str()])
            .args(["--namespace", installed.namespace.as_str()]);
        if let Some(version) = &package.version {
            cmd = cmd.args(["--version", version]);
        }
        if ctx.dry_run {
            cmd = cmd.arg("--dry-run");
        }
        self.run(cmd, ctx).await
    }
}

fn release_locator(locator: &str) -> (Option<&str>, &str) {
    locator
        .split_once('/')
        .map_or((None, locator), |(namespace, release)| {
            (Some(namespace), release)
        })
}

fn install_target(name: &str) -> (Option<(Option<&str>, &str)>, &str) {
    match name.split_once('=') {
        Some((locator, chart)) => (Some(release_locator(locator)), chart),
        None => (None, name),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "Helm"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::Other
    }

    fn database_id(&self) -> &'static str {
        "helm"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
            | Capabilities::SEARCH
            | Capabilities::REFRESH
            | Capabilities::UPGRADE
            | Capabilities::LIST_OUTDATED
            | Capabilities::PIN_VERSION
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        // One release per command: release names, namespaces, and versions
        // are all per-chart options.
        for package in packages {
            let (release, chart) = install_target(&package.name);
            let mut cmd = match release {
                Some((namespace, release)) => {
                    let mut cmd = self.cmd().args(["install", release, chart]);
                    if let Some(namespace) = namespace {
                        cmd = cmd.args(["--namespace", namespace]);
                    }
                    cmd
                }
                None => self.cmd().args(["install", "--generate-name", chart]),
            };
            if let Some(version) = &package.version {
                cmd = cmd.args(["--version", version]);
            }
            if ctx.dry_run {
                cmd = cmd.arg("--dry-run");
            }
            self.run(cmd, ctx).await?;
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        for package in packages {
            let installed = self.release(&package.name).await?;
            let mut cmd = self
                .cmd()
                .args(["uninstall", installed.name.as_str()])
                .args(["--namespace", installed.namespace.as_str()]);
            if ctx.dry_run {
                cmd = cmd.arg("--dry-run");
            }
            self.run(cmd, ctx).await?;
        }
        Ok(())
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.installed().await?))
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        match self.release(name).await {
            Ok(release) => return Ok(Box::new(release)),
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        self.search_repo(name)
            .await?
            .into_iter()
            .find(|package| package.name == name)
            .map(|package| Box::new(package) as Box<dyn Package>)
            .ok_or_else(|| Error::NotFound(name.into()))
    }

    async fn search(&self, query: &str) -> Result<Vec<Box<dyn Package>>> {
        Ok(boxed(self.search_repo(query).await?))
    }

    async fn refresh(&self, ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(Error::Other(format!("{ID}: refresh has no dry-run mode")));
        }
        self.run(self.cmd().args(["repo", "update"]), ctx).await
    }

    async fn upgrade(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if packages.is_empty() {
            let installed = self.installed().await?;
            for release in installed {
                self.upgrade_one(
                    &PackageRequest {
                        name: format!("{}/{}", release.namespace, release.name),
                        version: None,
                    },
                    ctx,
                )
                .await?;
            }
            return Ok(());
        }
        for package in packages {
            self.upgrade_one(package, ctx).await?;
        }
        Ok(())
    }

    async fn list_outdated(&self) -> Result<Vec<Box<dyn Package>>> {
        let mut outdated = Vec::new();
        for mut release in self.installed().await? {
            let Some(chart) = release.chart.clone() else {
                continue;
            };
            let Some(candidate) = self.search_repo(&chart).await?.into_iter().find(|package| {
                package
                    .name
                    .rsplit_once('/')
                    .map_or(package.name.as_str(), |(_, name)| name)
                    == chart
            }) else {
                continue;
            };
            if release.version != candidate.version {
                release.latest_version = candidate.version;
                release.state = InstallState::Upgradable;
                outdated.push(release);
            }
        }
        Ok(boxed(outdated))
    }
}

fn parse_release_json(stdout: &str) -> Result<Vec<HelmPackage>> {
    if stdout.trim().is_empty() {
        return Ok(Vec::new());
    }
    let rows: Value = serde_json::from_str(stdout).map_err(|error| Error::Parse {
        what: format!("{ID} list JSON"),
        detail: error.to_string(),
    })?;
    let rows = rows.as_array().ok_or_else(|| Error::Parse {
        what: format!("{ID} list JSON"),
        detail: "top-level value is not an array".into(),
    })?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let name = row.get("name")?.as_str()?.to_string();
            let namespace = row
                .get("namespace")
                .and_then(Value::as_str)
                .unwrap_or("default")
                .to_string();
            let chart_ref = row.get("chart").and_then(Value::as_str).unwrap_or("");
            let (chart, version) = split_chart_version(chart_ref);
            let app_version = row
                .get("app_version")
                .or_else(|| row.get("appVersion"))
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(HelmPackage {
                name,
                version,
                latest_version: None,
                description: app_version.as_ref().map(|version| format!("app {version}")),
                namespace,
                chart,
                app_version,
                state: InstallState::Installed,
            })
        })
        .collect())
}

fn parse_search_json(stdout: &str) -> Result<Vec<HelmPackage>> {
    if stdout.trim().is_empty() {
        return Ok(Vec::new());
    }
    let rows: Value = serde_json::from_str(stdout).map_err(|error| Error::Parse {
        what: format!("{ID} search JSON"),
        detail: error.to_string(),
    })?;
    let rows = rows.as_array().ok_or_else(|| Error::Parse {
        what: format!("{ID} search JSON"),
        detail: "top-level value is not an array".into(),
    })?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let name = row.get("name")?.as_str()?.to_string();
            let version = row
                .get("version")
                .or_else(|| row.get("chart_version"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let app_version = row
                .get("app_version")
                .or_else(|| row.get("appVersion"))
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(HelmPackage {
                chart: name.rsplit_once('/').map(|(_, chart)| chart.to_string()),
                name,
                version,
                latest_version: None,
                description: row
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                namespace: String::new(),
                app_version,
                state: InstallState::Available,
            })
        })
        .collect())
}

fn split_chart_version(value: &str) -> (Option<String>, Option<String>) {
    for (index, character) in value.char_indices().rev() {
        if character == '-'
            && value[index + 1..]
                .chars()
                .next()
                .is_some_and(|next| next.is_ascii_digit())
        {
            return (
                Some(value[..index].to_string()),
                Some(value[index + 1..].to_string()),
            );
        }
    }
    ((!value.is_empty()).then(|| value.to_string()), None)
}

fn boxed(packages: Vec<HelmPackage>) -> Vec<Box<dyn Package>> {
    packages
        .into_iter()
        .map(|package| Box::new(package) as Box<dyn Package>)
        .collect()
}

/// A deployed Helm release or an available repository chart.
#[derive(Debug)]
pub struct HelmPackage {
    pub name: String,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub description: Option<String>,
    pub namespace: String,
    pub chart: Option<String>,
    pub app_version: Option<String>,
    pub state: InstallState,
}

impl Package for HelmPackage {
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

    fn origin(&self) -> Option<&str> {
        (!self.namespace.is_empty()).then_some(self.namespace.as_str())
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_install_and_release_targets() {
        assert_eq!(install_target("bitnami/nginx"), (None, "bitnami/nginx"));
        assert_eq!(
            install_target("apps/web=bitnami/nginx"),
            (Some((Some("apps"), "web")), "bitnami/nginx")
        );
        assert_eq!(release_locator("apps/web"), (Some("apps"), "web"));
    }

    #[test]
    fn splits_chart_versions_from_the_right() {
        assert_eq!(
            split_chart_version("external-dns-1.15.2"),
            (Some("external-dns".into()), Some("1.15.2".into()))
        );
        assert_eq!(
            split_chart_version("odd-chart"),
            (Some("odd-chart".into()), None)
        );
    }

    #[test]
    fn parses_release_inventory() {
        let json = r#"[{"name":"web","namespace":"apps","status":"deployed","chart":"nginx-18.2.4","app_version":"1.27.3"}]"#;
        let packages = parse_release_json(json).unwrap();
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "web");
        assert_eq!(packages[0].chart.as_deref(), Some("nginx"));
        assert_eq!(packages[0].version.as_deref(), Some("18.2.4"));
        assert_eq!(packages[0].namespace, "apps");
    }

    #[test]
    fn parses_repository_search() {
        let json = r#"[{"name":"bitnami/nginx","version":"18.2.4","app_version":"1.27.3","description":"NGINX server"}]"#;
        let packages = parse_search_json(json).unwrap();
        assert_eq!(packages[0].name, "bitnami/nginx");
        assert_eq!(packages[0].chart.as_deref(), Some("nginx"));
        assert_eq!(packages[0].description.as_deref(), Some("NGINX server"));
    }
}
