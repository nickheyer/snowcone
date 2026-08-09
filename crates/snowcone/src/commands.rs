//! CLI command dispatch: pick backends, fan operations out, aggregate.

use anyhow::bail;
use serde::Serialize;
use snowcone_core::{
    Detection, Error, HostInfo, OpContext, Operation, PackageManager, PackageRequest,
    PackageSummary, Registry,
};

use crate::output;

/// Status of one registered backend on this host, shared by
/// `snow managers` and the TUI sidebar.
#[derive(Clone, Debug, Serialize)]
pub struct ManagerStatus {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub available: bool,
    /// Executable path when available, otherwise the reason it is not.
    pub detail: String,
    pub capabilities: Vec<String>,
}

/// Probe all registered backends, returning display rows plus the usable
/// manager instances.
pub fn manager_statuses(
    registry: &Registry,
    host: &HostInfo,
) -> (Vec<ManagerStatus>, Vec<Box<dyn PackageManager>>) {
    let mut rows = Vec::new();
    let mut managers = Vec::new();
    for probe in registry.probe(host) {
        let id = probe.factory.id().to_string();
        match probe.detection {
            Detection::Available { program } => match probe.factory.create(host) {
                Ok(manager) => {
                    rows.push(ManagerStatus {
                        id,
                        kind: Some(manager.kind().to_string()),
                        available: true,
                        detail: program.display().to_string(),
                        capabilities: manager.capabilities().names(),
                    });
                    managers.push(manager);
                }
                Err(error) => rows.push(ManagerStatus {
                    id,
                    kind: None,
                    available: false,
                    detail: format!("failed to initialize: {error}"),
                    capabilities: Vec::new(),
                }),
            },
            Detection::Unavailable { reason } => rows.push(ManagerStatus {
                id,
                kind: None,
                available: false,
                detail: reason,
                capabilities: Vec::new(),
            }),
        }
    }
    (rows, managers)
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
    /// The backends this invocation may touch.
    fn selected(&self) -> anyhow::Result<Vec<Box<dyn PackageManager>>> {
        if self.filter.is_empty() {
            return Ok(self.registry.discover(&self.host));
        }
        let mut managers = Vec::new();
        for id in &self.filter {
            let Some(factory) = self
                .registry
                .factories()
                .iter()
                .find(|factory| factory.id() == id)
            else {
                let known: Vec<_> = self
                    .registry
                    .factories()
                    .iter()
                    .map(|factory| factory.id())
                    .collect();
                bail!(
                    "unknown backend `{id}` (registered: {})",
                    if known.is_empty() {
                        "none yet".to_string()
                    } else {
                        known.join(", ")
                    }
                );
            };
            match factory.detect(&self.host) {
                Detection::Available { .. } => managers.push(factory.create(&self.host)?),
                Detection::Unavailable { reason } => {
                    bail!("backend `{id}` is not available on this host: {reason}")
                }
            }
        }
        Ok(managers)
    }

    /// Selected backends that support `operation`. Silently skips
    /// non-supporting backends unless they were requested by name.
    fn supporting(&self, operation: Operation) -> anyhow::Result<Vec<Box<dyn PackageManager>>> {
        let selected = self.selected()?;
        if selected.is_empty() {
            bail!(
                "no package manager backends detected ({} registered)",
                self.registry.factories().len()
            );
        }
        let explicit = !self.filter.is_empty();
        let mut managers = Vec::new();
        for manager in selected {
            if manager.supports(operation) {
                managers.push(manager);
            } else if explicit {
                bail!("`{}` does not support {operation}", manager.id());
            } else {
                tracing::debug!(manager = manager.id(), %operation, "skipping unsupported backend");
            }
        }
        if managers.is_empty() {
            bail!("no detected backend supports {operation}");
        }
        Ok(managers)
    }

    /// Mutating operations act through exactly one backend; make the user
    /// pick when the target is ambiguous.
    fn one_target(&self, operation: Operation) -> anyhow::Result<Box<dyn PackageManager>> {
        let mut managers = self.supporting(operation)?;
        if managers.len() > 1 {
            let ids: Vec<_> = managers.iter().map(|manager| manager.id()).collect();
            bail!(
                "multiple backends can {operation} ({}); pick one with --manager",
                ids.join(", ")
            );
        }
        Ok(managers.remove(0))
    }

    pub async fn search(&self, query: &str) -> anyhow::Result<()> {
        let mut results = Vec::new();
        for manager in self.supporting(Operation::Search)? {
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
        let mut found = Vec::new();
        for manager in self.supporting(Operation::Info)? {
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
        let mut results = Vec::new();
        for manager in self.supporting(operation)? {
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
        let mut failures = Vec::new();
        for manager in self.supporting(Operation::Refresh)? {
            match manager.refresh(&self.op_ctx).await {
                Ok(()) => println!("refreshed {}", manager.id()),
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
        let manager = self.one_target(Operation::Install)?;
        manager.install(&requests, &self.op_ctx).await?;
        Ok(())
    }

    pub async fn remove(&self, specs: &[String]) -> anyhow::Result<()> {
        let requests = parse_requests(specs);
        let manager = self.one_target(Operation::Remove)?;
        manager.remove(&requests, &self.op_ctx).await?;
        Ok(())
    }

    pub async fn upgrade(&self, specs: &[String]) -> anyhow::Result<()> {
        if specs.is_empty() {
            // `snow upgrade` upgrades everything, everywhere.
            let mut failures = Vec::new();
            for manager in self.supporting(Operation::Upgrade)? {
                match manager.upgrade(&[], &self.op_ctx).await {
                    Ok(()) => println!("upgraded {}", manager.id()),
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
        let manager = self.one_target(Operation::Upgrade)?;
        manager.upgrade(&requests, &self.op_ctx).await?;
        Ok(())
    }

    pub fn managers(&self) -> anyhow::Result<()> {
        let (rows, _) = manager_statuses(&self.registry, &self.host);
        output::managers(&rows, self.json)
    }
}

fn parse_requests(specs: &[String]) -> Vec<PackageRequest> {
    specs.iter().map(|spec| PackageRequest::parse(spec)).collect()
}
