//! Read-operation spawners: search fan-out, installed/outdated listing,
//! and info enrichment. All epoch-tagged (stale results are dropped by
//! the App) and cancellable by aborting the returned handle - capture
//! children die with the future (`kill_on_drop`).

use std::collections::BTreeSet;
use std::sync::Arc;

use snowcone_core::{DatabaseGroup, Operation, PackageSummary};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::{JoinHandle, JoinSet};

use super::app::TuiMsg;
use super::packages::PkgKey;
use super::pool::elect_enabled;
use super::tasks::TaskId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListTarget {
    Installed,
    Outdated,
}

impl ListTarget {
    pub fn label(self) -> &'static str {
        match self {
            ListTarget::Installed => "installed",
            ListTarget::Outdated => "outdated",
        }
    }
}

/// Fan a query out to every enabled database's elected search manager.
/// A non-empty `restrict` (the query's `@manager` tokens) narrows the
/// fan-out to those manager ids. Each database reports as it finishes
/// (`SearchBatch`); the supervisor closes with `SearchDone` carrying
/// per-manager failures.
pub fn spawn_search(
    groups: Arc<Vec<DatabaseGroup>>,
    disabled: BTreeSet<String>,
    restrict: BTreeSet<String>,
    query: String,
    epoch: u64,
    task: TaskId,
    tx: UnboundedSender<TuiMsg>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut set = JoinSet::new();
        for index in 0..groups.len() {
            let groups = Arc::clone(&groups);
            let disabled = disabled.clone();
            let restrict = restrict.clone();
            let query = query.clone();
            let tx = tx.clone();
            set.spawn(async move {
                let group = &groups[index];
                let manager = elect_search(group, &disabled, &restrict)?;
                match manager.search(&query).await {
                    Ok(packages) => {
                        let packages = packages
                            .iter()
                            .map(|package| PackageSummary::new(package.as_ref()))
                            .collect();
                        let _ = tx.send(TuiMsg::SearchBatch { epoch, packages });
                        None
                    }
                    Err(error) => Some(format!("{}: {error}", manager.id())),
                }
            });
        }
        let mut errors = Vec::new();
        while let Some(result) = set.join_next().await {
            if let Ok(Some(error)) = result {
                errors.push(error);
            }
        }
        let _ = tx.send(TuiMsg::SearchDone {
            task,
            epoch,
            errors,
        });
    })
}

/// [`elect_enabled`] narrowed to the ids the query's `@manager` tokens
/// named (all of them when none were).
fn elect_search<'a>(
    group: &'a DatabaseGroup,
    disabled: &BTreeSet<String>,
    restrict: &BTreeSet<String>,
) -> Option<&'a dyn snowcone_core::PackageManager> {
    group
        .managers
        .iter()
        .map(|manager| manager.as_ref())
        .filter(|manager| !disabled.contains(manager.id()))
        .filter(|manager| restrict.is_empty() || restrict.contains(manager.id()))
        .find(|manager| manager.supports(Operation::Search))
}

/// Aggregate installed / outdated packages across every enabled
/// database's elected manager, in parallel.
pub fn spawn_list(
    groups: Arc<Vec<DatabaseGroup>>,
    disabled: BTreeSet<String>,
    target: ListTarget,
    epoch: u64,
    task: TaskId,
    tx: UnboundedSender<TuiMsg>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let operation = match target {
            ListTarget::Installed => Operation::ListInstalled,
            ListTarget::Outdated => Operation::ListOutdated,
        };
        let mut set = JoinSet::new();
        for index in 0..groups.len() {
            let groups = Arc::clone(&groups);
            let disabled = disabled.clone();
            set.spawn(async move {
                let group = &groups[index];
                let Some(manager) = elect_enabled(group, operation, &disabled) else {
                    return (Vec::new(), None);
                };
                let listed = match target {
                    ListTarget::Installed => manager.list_installed().await,
                    ListTarget::Outdated => manager.list_outdated().await,
                };
                match listed {
                    Ok(packages) => (
                        packages
                            .iter()
                            .map(|package| PackageSummary::new(package.as_ref()))
                            .collect(),
                        None,
                    ),
                    Err(error) => (Vec::new(), Some(format!("{}: {error}", manager.id()))),
                }
            });
        }
        let mut packages = Vec::new();
        let mut errors = Vec::new();
        while let Some(result) = set.join_next().await {
            if let Ok((listed, error)) = result {
                packages.extend(listed);
                errors.extend(error);
            }
        }
        let _ = tx.send(TuiMsg::Listed {
            task,
            target,
            epoch,
            packages,
            errors,
        });
    })
}

/// Fetch full metadata for one package, for the detail pane. Prefers the
/// row's own manager when it is enabled and can answer; falls back to
/// the database's elected Info manager.
pub fn spawn_info(
    groups: Arc<Vec<DatabaseGroup>>,
    disabled: BTreeSet<String>,
    key: PkgKey,
    tx: UnboundedSender<TuiMsg>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (manager_id, name) = key.clone();
        let result = info_for(&groups, &disabled, &manager_id, &name)
            .await
            .map(Box::new);
        let _ = tx.send(TuiMsg::Info { key, result });
    })
}

async fn info_for(
    groups: &[DatabaseGroup],
    disabled: &BTreeSet<String>,
    manager_id: &str,
    name: &str,
) -> Result<PackageSummary, String> {
    let group = groups
        .iter()
        .find(|group| {
            group
                .managers
                .iter()
                .any(|manager| manager.id() == manager_id)
        })
        .ok_or_else(|| format!("backend `{manager_id}` is no longer detected"))?;
    let own = group
        .managers
        .iter()
        .map(|manager| manager.as_ref())
        .find(|manager| manager.id() == manager_id)
        .filter(|manager| !disabled.contains(manager.id()) && manager.supports(Operation::Info));
    let manager = own
        .or_else(|| elect_enabled(group, Operation::Info, disabled))
        .ok_or_else(|| format!("no enabled manager in [{}] supports info", group.database))?;
    manager
        .info(name)
        .await
        .map(|package| PackageSummary::new(package.as_ref()))
        .map_err(|error| format!("{}: {error}", manager.id()))
}
