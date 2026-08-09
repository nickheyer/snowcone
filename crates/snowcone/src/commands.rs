//! CLI command dispatch: discover backends, group them by package database,
//! elect one member per operation, and aggregate results.

use std::collections::HashMap;

use anyhow::bail;
use serde::Serialize;
use snowcone_core::{
    DatabaseGroup, Detection, ElevationSession, Elevator, Error, HostInfo, InstallState,
    OpContext, Operation, PackageManager, PackageRequest, PackageSummary, Registry,
    group_by_database,
};

use crate::{output, picker, relevance};

/// Status of one registered backend on this host, shared by
/// `snow managers` and the TUI sidebar.
#[derive(Clone, Debug, Serialize)]
pub struct ManagerStatus {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    pub available: bool,
    /// Highest-preference member of its database group.
    pub primary: bool,
    /// Executable path when available, otherwise the reason it is not.
    pub detail: String,
    pub capabilities: Vec<String>,
}

/// Probe all registered backends, returning display rows (available ones
/// first, in group order) plus the grouped manager instances.
pub fn manager_statuses(
    registry: &Registry,
    host: &HostInfo,
) -> (Vec<ManagerStatus>, Vec<DatabaseGroup>) {
    let mut unavailable = Vec::new();
    let mut managers = Vec::new();
    let mut programs: HashMap<String, String> = HashMap::new();
    for probe in registry.probe(host) {
        let id = probe.factory.id().to_string();
        match probe.detection {
            Detection::Available { program } => match probe.factory.create(host) {
                Ok(manager) => {
                    programs.insert(id, program.display().to_string());
                    managers.push(manager);
                }
                Err(error) => unavailable.push(ManagerStatus {
                    id,
                    kind: None,
                    database: None,
                    available: false,
                    primary: false,
                    detail: format!("failed to initialize: {error}"),
                    capabilities: Vec::new(),
                }),
            },
            Detection::Unavailable { reason } => unavailable.push(ManagerStatus {
                id,
                kind: None,
                database: None,
                available: false,
                primary: false,
                detail: reason,
                capabilities: Vec::new(),
            }),
        }
    }
    let groups = group_by_database(managers);
    let mut rows = Vec::new();
    for group in &groups {
        for (index, manager) in group.managers.iter().enumerate() {
            rows.push(ManagerStatus {
                id: manager.id().to_string(),
                kind: Some(manager.kind().to_string()),
                database: Some(group.database.to_string()),
                available: true,
                primary: index == 0,
                detail: programs.get(manager.id()).cloned().unwrap_or_default(),
                capabilities: manager.capabilities().names(),
            });
        }
    }
    unavailable.sort_by(|a, b| a.id.cmp(&b.id));
    rows.extend(unavailable);
    (rows, groups)
}

pub struct Runner {
    pub host: HostInfo,
    pub registry: Registry,
    /// `--manager` ids; empty means every detected backend.
    pub filter: Vec<String>,
    pub json: bool,
    pub op_ctx: OpContext,
}

impl Runner {
    /// Detected managers this invocation may touch, grouped by database.
    /// A `--manager` filter shrinks the pool before grouping, so an
    /// explicitly requested tool becomes its group's primary.
    fn groups(&self) -> anyhow::Result<Vec<DatabaseGroup>> {
        let managers = if self.filter.is_empty() {
            self.registry.discover(&self.host)
        } else {
            let mut managers: Vec<Box<dyn PackageManager>> = Vec::new();
            for id in &self.filter {
                let Some(factory) = self
                    .registry
                    .factories()
                    .iter()
                    .find(|factory| factory.id() == id)
                else {
                    bail!("unknown backend `{id}` (see `snow managers` for the full list)");
                };
                match factory.detect(&self.host) {
                    Detection::Available { .. } => managers.push(factory.create(&self.host)?),
                    Detection::Unavailable { reason } => {
                        bail!("backend `{id}` is not available on this host: {reason}")
                    }
                }
            }
            managers
        };
        if managers.is_empty() {
            bail!(
                "no package manager backends detected ({} registered)",
                self.registry.factories().len()
            );
        }
        Ok(group_by_database(managers))
    }

    /// One elected manager per database group that can perform `operation`.
    /// Groups with no capable member are skipped, unless the user asked for
    /// their tools by name.
    fn elect<'a>(
        &self,
        groups: &'a [DatabaseGroup],
        operation: Operation,
    ) -> anyhow::Result<Vec<&'a dyn PackageManager>> {
        let explicit = !self.filter.is_empty();
        let mut elected = Vec::new();
        for group in groups {
            match group.elect(operation) {
                Some(manager) => elected.push(manager),
                None if explicit => {
                    let ids: Vec<_> = group.managers.iter().map(|m| m.id()).collect();
                    bail!("{} does not support {operation}", ids.join(", "));
                }
                None => tracing::debug!(
                    database = group.database,
                    %operation,
                    "no member supports operation; skipping database",
                ),
            }
        }
        if elected.is_empty() {
            bail!("no detected backend supports {operation}");
        }
        Ok(elected)
    }

    /// Which of the elected managers actually know `name`, probed via
    /// `info`. Database order is preserved, so index 0 is the default pick.
    async fn locate<'a>(
        &self,
        elected: &[&'a dyn PackageManager],
        name: &str,
    ) -> Vec<(&'a dyn PackageManager, PackageSummary)> {
        let mut found = Vec::new();
        for manager in elected {
            match manager.info(name).await {
                Ok(package) => found.push((*manager, PackageSummary::new(package.as_ref()))),
                Err(Error::NotFound(_)) => {}
                Err(error) => {
                    tracing::debug!(manager = manager.id(), %error, "info probe failed");
                }
            }
        }
        found
    }

    /// Resolve every request to one manager, yay-style: probe which
    /// databases know the package, take single hits automatically, and open
    /// the numbered picker when several match (choosing the package chooses
    /// its manager). Returns per-manager batches in pick order.
    async fn assign<'a>(
        &self,
        groups: &'a [DatabaseGroup],
        operation: Operation,
        requests: Vec<PackageRequest>,
        prefer_installed: bool,
    ) -> anyhow::Result<Vec<(&'a dyn PackageManager, Vec<PackageRequest>)>> {
        let elected = self.elect(groups, operation)?;
        let mut batches: Vec<(&'a dyn PackageManager, Vec<PackageRequest>)> = Vec::new();
        let mut assign = |manager: &'a dyn PackageManager, request: PackageRequest| {
            match batches
                .iter_mut()
                .find(|(assigned, _)| assigned.id() == manager.id())
            {
                Some((_, list)) => list.push(request),
                None => batches.push((manager, vec![request])),
            }
        };
        // One capable manager (usually an explicit --manager): nothing to
        // resolve, no probes.
        if elected.len() == 1 {
            for request in requests {
                assign(elected[0], request);
            }
            return Ok(batches);
        }
        for request in requests {
            let mut candidates = self.locate(&elected, &request.name).await;
            // Remove/upgrade want the copy that is actually installed;
            // fall back to every match when no probe reports one (the
            // chosen backend still refuses cleanly if it is truly absent).
            if prefer_installed {
                let installed: Vec<_> = candidates
                    .iter()
                    .filter(|(_, summary)| {
                        matches!(
                            summary.state,
                            InstallState::Installed | InstallState::Upgradable
                        )
                    })
                    .map(|(manager, summary)| (*manager, summary.clone()))
                    .collect();
                if !installed.is_empty() {
                    candidates = installed;
                }
            }
            let manager = match candidates.len() {
                0 => {
                    let ids: Vec<&str> = elected.iter().map(|manager| manager.id()).collect();
                    bail!(
                        "`{}` not found by any capable manager ({}) - try `snow search {}`",
                        request.name,
                        ids.join(", "),
                        request.name
                    );
                }
                1 => candidates[0].0,
                _ => {
                    let summaries: Vec<PackageSummary> = candidates
                        .iter()
                        .map(|(_, summary)| summary.clone())
                        .collect();
                    let headline = format!(
                        "`{}` matches in {} managers",
                        request.name,
                        candidates.len()
                    );
                    let choice =
                        picker::pick(headline, &summaries, self.op_ctx.assume_yes).await?;
                    candidates[choice].0
                }
            };
            assign(manager, request);
        }
        Ok(batches)
    }

    /// Validate credentials once, up front, when any manager in the run
    /// needs them: one password prompt for the whole batch, kept warm by
    /// the session so it cannot expire mid-build (see [`Elevator::hold`]).
    async fn hold_elevation<'a>(
        &self,
        mut managers: impl Iterator<Item = &'a dyn PackageManager>,
        operation: Operation,
    ) -> anyhow::Result<Option<ElevationSession>> {
        if self.host.is_root || !managers.any(|manager| manager.needs_elevation(operation)) {
            return Ok(None);
        }
        Ok(Some(Elevator::detect(&self.host).hold().await?))
    }

    pub async fn search(&self, query: &str) -> anyhow::Result<()> {
        let groups = self.groups()?;
        let mut results = Vec::new();
        for manager in self.elect(&groups, Operation::Search)? {
            match manager.search(query).await {
                Ok(packages) => results.extend(
                    packages
                        .iter()
                        .map(|package| PackageSummary::new(package.as_ref())),
                ),
                Err(error) => {
                    tracing::warn!(manager = manager.id(), %error, "search failed");
                }
            }
        }
        // Best matches first: exact name, then prefix, then substring -
        // not the alphabet, which buries `vim` under 400 a-through-u hits.
        results.sort_by(|a, b| {
            relevance::rank(&a.name, query)
                .cmp(&relevance::rank(&b.name, query))
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.manager.cmp(&b.manager))
        });
        output::packages(&results, self.json)
    }

    pub async fn info(&self, name: &str) -> anyhow::Result<()> {
        let groups = self.groups()?;
        let mut found = Vec::new();
        for manager in self.elect(&groups, Operation::Info)? {
            match manager.info(name).await {
                Ok(package) => found.push(PackageSummary::new(package.as_ref())),
                Err(Error::NotFound(_)) => {}
                Err(error) => {
                    tracing::warn!(manager = manager.id(), %error, "info failed");
                }
            }
        }
        if found.is_empty() {
            bail!("package `{name}` not found in any detected manager");
        }
        if self.json {
            output::json(&found)
        } else {
            for package in &found {
                output::details(package);
            }
            Ok(())
        }
    }

    pub async fn list(&self, outdated: bool) -> anyhow::Result<()> {
        let operation = if outdated {
            Operation::ListOutdated
        } else {
            Operation::ListInstalled
        };
        let groups = self.groups()?;
        let mut results = Vec::new();
        for manager in self.elect(&groups, operation)? {
            let listed = if outdated {
                manager.list_outdated().await
            } else {
                manager.list_installed().await
            };
            match listed {
                Ok(packages) => results.extend(
                    packages
                        .iter()
                        .map(|package| PackageSummary::new(package.as_ref())),
                ),
                Err(error) => {
                    tracing::warn!(manager = manager.id(), %error, "listing failed");
                }
            }
        }
        results.sort_by(|a, b| a.manager.cmp(&b.manager).then(a.name.cmp(&b.name)));
        output::packages(&results, self.json)
    }

    pub async fn refresh(&self) -> anyhow::Result<()> {
        let groups = self.groups()?;
        let elected = self.elect(&groups, Operation::Refresh)?;
        let _session = self
            .hold_elevation(elected.iter().copied(), Operation::Refresh)
            .await?;
        let mut failures = Vec::new();
        for manager in elected {
            match manager.refresh(&self.op_ctx).await {
                Ok(()) => println!("refreshed {} [{}]", manager.id(), manager.database_id()),
                Err(error) => {
                    eprintln!("refresh failed for {}: {error}", manager.id());
                    failures.push(manager.id());
                }
            }
        }
        if !failures.is_empty() {
            bail!("refresh failed for: {}", failures.join(", "));
        }
        Ok(())
    }

    pub async fn install(&self, specs: &[String]) -> anyhow::Result<()> {
        let requests = parse_requests(specs);
        let groups = self.groups()?;
        let batches = self
            .assign(&groups, Operation::Install, requests, false)
            .await?;
        let _session = self
            .hold_elevation(
                batches.iter().map(|(manager, _)| *manager),
                Operation::Install,
            )
            .await?;
        for (manager, requests) in &batches {
            manager.install(requests, &self.op_ctx).await?;
        }
        Ok(())
    }

    pub async fn remove(&self, specs: &[String]) -> anyhow::Result<()> {
        let requests = parse_requests(specs);
        let groups = self.groups()?;
        let batches = self
            .assign(&groups, Operation::Remove, requests, true)
            .await?;
        let _session = self
            .hold_elevation(
                batches.iter().map(|(manager, _)| *manager),
                Operation::Remove,
            )
            .await?;
        for (manager, requests) in &batches {
            manager.remove(requests, &self.op_ctx).await?;
        }
        Ok(())
    }

    pub async fn upgrade(&self, specs: &[String]) -> anyhow::Result<()> {
        let groups = self.groups()?;
        if specs.is_empty() {
            // `snow upgrade` upgrades everything: each database once,
            // through its elected manager, with credentials validated once
            // up front instead of one sudo prompt per database.
            let elected = self.elect(&groups, Operation::Upgrade)?;
            let _session = self
                .hold_elevation(elected.iter().copied(), Operation::Upgrade)
                .await?;
            let mut failures = Vec::new();
            for manager in elected {
                match manager.upgrade(&[], &self.op_ctx).await {
                    Ok(()) => println!("upgraded {} [{}]", manager.id(), manager.database_id()),
                    Err(error) => {
                        eprintln!("upgrade failed for {}: {error}", manager.id());
                        failures.push(manager.id());
                    }
                }
            }
            if !failures.is_empty() {
                bail!("upgrade failed for: {}", failures.join(", "));
            }
            return Ok(());
        }
        let requests = parse_requests(specs);
        let batches = self
            .assign(&groups, Operation::Upgrade, requests, true)
            .await?;
        let _session = self
            .hold_elevation(
                batches.iter().map(|(manager, _)| *manager),
                Operation::Upgrade,
            )
            .await?;
        for (manager, requests) in &batches {
            manager.upgrade(requests, &self.op_ctx).await?;
        }
        Ok(())
    }

    pub fn managers(&self) -> anyhow::Result<()> {
        let (rows, _) = manager_statuses(&self.registry, &self.host);
        output::managers(&rows, self.json)
    }
}

fn parse_requests(specs: &[String]) -> Vec<PackageRequest> {
    specs
        .iter()
        .map(|spec| PackageRequest::parse(spec))
        .collect()
}
