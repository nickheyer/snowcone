//! The TUI's view of the detected managers: database groups plus the
//! user's persisted disable set, and mutation planning against them.
//!
//! Election mirrors `DatabaseGroup::elect` but skips disabled ids -
//! TUI-side, so disabled-manager state never leaks into snowcone-core.
//! Groups are snapshots behind an `Arc`: in-flight tasks keep the set
//! they started with; a re-probe swaps a fresh one in.

use std::collections::BTreeSet;
use std::sync::Arc;

use snowcone_core::{DatabaseGroup, Operation, PackageManager, PackageRequest, PackageSummary};

use super::policy::{self, ExecMode};

/// Everything the confirm modal shows and the executor needs: planned
/// once, confirmed by the user, then run exactly as confirmed.
#[derive(Clone, Debug)]
pub struct MutationPlan {
    pub operation: Operation,
    pub database: &'static str,
    /// Elected at plan time; the executor runs THIS manager, not a
    /// re-election.
    pub manager_id: String,
    pub title: String,
    /// Empty means "everything" (upgrade-all, refresh).
    pub requests: Vec<PackageRequest>,
    pub needs_elevation: bool,
    pub mode: ExecMode,
}

pub struct ManagerPool {
    pub groups: Arc<Vec<DatabaseGroup>>,
}

/// `DatabaseGroup::elect`, minus disabled managers.
pub fn elect_enabled<'a>(
    group: &'a DatabaseGroup,
    operation: Operation,
    disabled: &BTreeSet<String>,
) -> Option<&'a dyn PackageManager> {
    group
        .managers
        .iter()
        .map(|manager| manager.as_ref())
        .filter(|manager| !disabled.contains(manager.id()))
        .find(|manager| manager.supports(operation))
}

impl ManagerPool {
    pub fn new(groups: Vec<DatabaseGroup>) -> Self {
        Self {
            groups: Arc::new(groups),
        }
    }

    pub fn swap(&mut self, groups: Vec<DatabaseGroup>) {
        self.groups = Arc::new(groups);
    }

    /// The manager a group's operations would actually go to with the
    /// current disable set - what the Managers tab stars.
    pub fn effective_primary(&self, database: &str, disabled: &BTreeSet<String>) -> Option<&str> {
        self.groups
            .iter()
            .find(|group| group.database == database)?
            .managers
            .iter()
            .map(|manager| manager.id())
            .find(|id| !disabled.contains(*id))
    }

    fn group_containing(&self, manager_id: &str) -> Option<&DatabaseGroup> {
        self.groups.iter().find(|group| {
            group
                .managers
                .iter()
                .any(|manager| manager.id() == manager_id)
        })
    }

    /// Plan a row-targeted mutation: all targets in one database (mirrors
    /// the CLI's `Runner::one` rule), elected within it. Errors are
    /// status-line strings.
    pub fn plan_mutation(
        &self,
        operation: Operation,
        targets: &[&PackageSummary],
        disabled: &BTreeSet<String>,
    ) -> Result<MutationPlan, String> {
        if targets.is_empty() {
            return Err("nothing selected".to_string());
        }
        let mut group: Option<&DatabaseGroup> = None;
        let mut databases: BTreeSet<&'static str> = BTreeSet::new();
        for target in targets {
            let owner = self
                .group_containing(&target.manager)
                .ok_or_else(|| format!("backend `{}` is no longer detected", target.manager))?;
            databases.insert(owner.database);
            group = Some(owner);
        }
        if databases.len() > 1 {
            let list: Vec<&str> = databases.into_iter().collect();
            return Err(format!(
                "selection spans multiple databases ({}) - run one at a time",
                list.join(", ")
            ));
        }
        let group = group.expect("targets is non-empty");
        let manager = elect_enabled(group, operation, disabled).ok_or_else(|| {
            match group.elect(operation) {
                Some(capable) => format!("{} is disabled (Managers tab)", capable.id()),
                None => format!("no manager in [{}] supports {operation}", group.database),
            }
        })?;
        let mut names: Vec<String> = Vec::new();
        for target in targets {
            if !names.contains(&target.name) {
                names.push(target.name.clone());
            }
        }
        let requests: Vec<PackageRequest> = names
            .iter()
            .map(|name| PackageRequest {
                name: name.clone(),
                version: None,
            })
            .collect();
        Ok(build_plan(
            operation,
            group.database,
            manager,
            requests,
            &names,
        ))
    }

    /// One plan per database for target-less operations (`upgrade`
    /// everything, `refresh` indexes).
    pub fn plan_all(
        &self,
        operation: Operation,
        disabled: &BTreeSet<String>,
    ) -> Result<Vec<MutationPlan>, String> {
        let plans: Vec<MutationPlan> = self
            .groups
            .iter()
            .filter_map(|group| {
                elect_enabled(group, operation, disabled)
                    .map(|manager| build_plan(operation, group.database, manager, Vec::new(), &[]))
            })
            .collect();
        if plans.is_empty() {
            return Err(format!("no enabled manager supports {operation}"));
        }
        Ok(plans)
    }
}

fn build_plan(
    operation: Operation,
    database: &'static str,
    manager: &dyn PackageManager,
    requests: Vec<PackageRequest>,
    names: &[String],
) -> MutationPlan {
    let needs_elevation = manager.needs_elevation(operation);
    // needs_elevation is a safety net over the policy table: it can only
    // force Interactive, never Captured.
    let mode = if needs_elevation {
        ExecMode::Interactive
    } else {
        policy::exec_mode(manager.id())
    };
    let title = if names.is_empty() {
        format!("{operation} everything ({})", manager.id())
    } else {
        format!("{operation} {} ({})", summarize(names), manager.id())
    };
    MutationPlan {
        operation,
        database,
        manager_id: manager.id().to_string(),
        title,
        requests,
        needs_elevation,
        mode,
    }
}

fn summarize(names: &[String]) -> String {
    const SHOWN: usize = 3;
    if names.len() <= SHOWN {
        return names.join(", ");
    }
    format!(
        "{} +{} more",
        names[..SHOWN].join(", "),
        names.len() - SHOWN
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use snowcone_core::{
        Capabilities, Error, InstallState, ManagerKind, OpContext, Package, Result,
        group_by_database,
    };

    #[derive(Debug)]
    struct FakeManager {
        id: &'static str,
        database: &'static str,
        capabilities: Capabilities,
        elevated: bool,
    }

    #[async_trait]
    impl PackageManager for FakeManager {
        fn id(&self) -> &'static str {
            self.id
        }
        fn display_name(&self) -> &'static str {
            self.id
        }
        fn kind(&self) -> ManagerKind {
            ManagerKind::Other
        }
        fn database_id(&self) -> &'static str {
            self.database
        }
        fn capabilities(&self) -> Capabilities {
            self.capabilities
        }
        fn needs_elevation(&self, operation: Operation) -> bool {
            self.elevated && operation.mutates()
        }
        async fn install(&self, _: &[PackageRequest], _: &OpContext) -> Result<()> {
            Err(Error::Other("test".into()))
        }
        async fn remove(&self, _: &[PackageRequest], _: &OpContext) -> Result<()> {
            Err(Error::Other("test".into()))
        }
        async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
            Ok(Vec::new())
        }
        async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
            Err(Error::NotFound(name.to_string()))
        }
    }

    fn pool() -> ManagerPool {
        let managers: Vec<Box<dyn PackageManager>> = vec![
            Box::new(FakeManager {
                id: "npm",
                database: "node",
                capabilities: Capabilities::CORE | Capabilities::SEARCH | Capabilities::UPGRADE,
                elevated: false,
            }),
            Box::new(FakeManager {
                id: "bun",
                database: "node",
                capabilities: Capabilities::CORE,
                elevated: false,
            }),
            Box::new(FakeManager {
                id: "apt",
                database: "dpkg",
                capabilities: Capabilities::CORE | Capabilities::REFRESH | Capabilities::UPGRADE,
                elevated: true,
            }),
        ];
        ManagerPool::new(group_by_database(managers))
    }

    fn row(manager: &str, name: &str) -> PackageSummary {
        PackageSummary {
            manager: manager.to_string(),
            name: name.to_string(),
            version: None,
            latest_version: None,
            description: None,
            homepage: None,
            license: None,
            architecture: None,
            origin: None,
            installed_size: None,
            download_size: None,
            dependencies: None,
            state: InstallState::Available,
        }
    }

    #[test]
    fn plans_a_single_database_batch() {
        let pool = pool();
        let a = row("npm", "typescript");
        let b = row("bun", "typescript");
        let c = row("npm", "eslint");
        let plan = pool
            .plan_mutation(Operation::Install, &[&a, &b, &c], &BTreeSet::new())
            .unwrap();
        assert_eq!(plan.database, "node");
        assert_eq!(plan.manager_id, "npm"); // preference order: npm over bun
        // Duplicate name across managers in one database deduplicates.
        assert_eq!(plan.requests.len(), 2);
        assert_eq!(plan.mode, ExecMode::Captured);
        assert!(!plan.needs_elevation);
    }

    #[test]
    fn rejects_cross_database_batches() {
        let pool = pool();
        let a = row("npm", "typescript");
        let b = row("apt", "ripgrep");
        let error = pool
            .plan_mutation(Operation::Install, &[&a, &b], &BTreeSet::new())
            .unwrap_err();
        assert!(error.contains("spans multiple databases"), "{error}");
        assert!(error.contains("dpkg") && error.contains("node"), "{error}");
    }

    #[test]
    fn disabled_manager_elects_the_next_member_or_explains() {
        let pool = pool();
        let a = row("npm", "typescript");
        let disabled: BTreeSet<String> = ["npm".to_string()].into();
        // bun can install, so election falls through to it.
        let plan = pool
            .plan_mutation(Operation::Install, &[&a], &disabled)
            .unwrap();
        assert_eq!(plan.manager_id, "bun");
        // But nothing else in [node] can upgrade: the error names the
        // disabled tool that could.
        let error = pool
            .plan_mutation(Operation::Upgrade, &[&a], &disabled)
            .unwrap_err();
        assert!(error.contains("npm is disabled"), "{error}");
    }

    #[test]
    fn elevation_forces_interactive_mode() {
        let pool = pool();
        let a = row("apt", "ripgrep");
        let plan = pool
            .plan_mutation(Operation::Install, &[&a], &BTreeSet::new())
            .unwrap();
        assert!(plan.needs_elevation);
        assert_eq!(plan.mode, ExecMode::Interactive);
    }

    #[test]
    fn plan_all_yields_one_plan_per_capable_database() {
        let pool = pool();
        let plans = pool.plan_all(Operation::Upgrade, &BTreeSet::new()).unwrap();
        assert_eq!(plans.len(), 2); // node (npm) + dpkg (apt)
        assert!(plans.iter().all(|plan| plan.requests.is_empty()));
        let error = pool
            .plan_all(Operation::Refresh, &["apt".to_string()].into())
            .unwrap_err();
        assert!(error.contains("no enabled manager"), "{error}");
    }

    #[test]
    fn effective_primary_moves_past_disabled_members() {
        let pool = pool();
        assert_eq!(
            pool.effective_primary("node", &BTreeSet::new()),
            Some("npm")
        );
        assert_eq!(
            pool.effective_primary("node", &["npm".to_string()].into()),
            Some("bun")
        );
    }
}
