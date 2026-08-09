//! CLI command dispatch: discover backends, group them by package database,
//! elect one member per operation, and aggregate results.

use std::collections::HashMap;

use anyhow::bail;
use serde::Serialize;
use snowcone_core::{
    DatabaseGroup, Detection, Error, HostInfo, OpContext, Operation, PackageManager,
    PackageRequest, PackageSummary, Registry, group_by_database,
};

use crate::output;

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

    /// Mutating operations touch exactly one database; make the user pick
    /// when several could satisfy the request.
    fn one<'a>(
        &self,
        groups: &'a [DatabaseGroup],
        operation: Operation,
    ) -> anyhow::Result<&'a dyn PackageManager> {
        let elected = self.elect(groups, operation)?;
        if elected.len() > 1 {
            let targets: Vec<String> = elected
                .iter()
                .map(|manager| format!("{} [{}]", manager.id(), manager.database_id()))
                .collect();
            bail!(
                "multiple package databases could handle {operation} ({}); pick one with --manager",
                targets.join(", ")
            );
        }
        Ok(elected[0])
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
        results.sort_by(|a, b| a.name.cmp(&b.name).then(a.manager.cmp(&b.manager)));
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
        let mut failures = Vec::new();
        for manager in self.elect(&groups, Operation::Refresh)? {
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
        let manager = self.one(&groups, Operation::Install)?;
        manager.install(&requests, &self.op_ctx).await?;
        Ok(())
    }

    pub async fn remove(&self, specs: &[String]) -> anyhow::Result<()> {
        let requests = parse_requests(specs);
        let groups = self.groups()?;
        let manager = self.one(&groups, Operation::Remove)?;
        manager.remove(&requests, &self.op_ctx).await?;
        Ok(())
    }

    pub async fn upgrade(&self, specs: &[String]) -> anyhow::Result<()> {
        let groups = self.groups()?;
        if specs.is_empty() {
            // `snow upgrade` upgrades everything: each database once,
            // through its elected manager.
            let mut failures = Vec::new();
            for manager in self.elect(&groups, Operation::Upgrade)? {
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
        let manager = self.one(&groups, Operation::Upgrade)?;
        manager.upgrade(&requests, &self.op_ctx).await?;
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
