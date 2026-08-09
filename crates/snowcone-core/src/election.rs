//! Database grouping and primary election.
//!
//! Many tools can drive the same package database: apt, nala, aptitude, and
//! dpkg all mutate the dpkg database; yay wraps pacman's; pip and uv share
//! site-packages. If snowcone treated each tool independently, `snow list`
//! would show every Debian package four times and `snow upgrade` would run
//! three full upgrades over one database.
//!
//! So every backend declares a [`database_id`](crate::PackageManager::database_id).
//! Detected managers sharing a database form a [`DatabaseGroup`], and each
//! operation is routed to the single highest-preference member that supports
//! it. The preference order lives in [`PREFERENCE`] — one auditable table,
//! not a convention spread across backend crates. `--manager` bypasses
//! election entirely by shrinking the group to the requested tools.

use crate::capability::Operation;
use crate::manager::PackageManager;

/// Election order per database: first entry present on the host wins.
/// Managers not listed rank after listed ones, in registration order.
/// Databases with only one tool don't need an entry.
pub const PREFERENCE: &[(&str, &[&str])] = &[
    (
        "dpkg",
        &["apt", "nala", "aptitude", "pacstall", "makedeb", "dpkg"],
    ),
    (
        "rpmdb",
        &[
            "rpm-ostree",
            "transactional-update",
            "dnf",
            "zypper",
            "yum",
            "urpmi",
            "apt-rpm",
            "rpm",
        ],
    ),
    (
        "alpm",
        &["paru", "yay", "pikaur", "trizen", "aura", "pamac", "pacman"],
    ),
    (
        "slackware",
        &[
            "slackpkg",
            "slapt-get",
            "slpkg",
            "sbopkg",
            "netpkg",
            "pkgtools",
        ],
    ),
    ("portage", &["emerge", "cave"]),
    ("conda", &["mamba", "conda"]),
    ("python", &["uv", "pipx", "pip", "poetry", "pdm", "hatch"]),
    ("node", &["pnpm", "npm", "yarn", "bun", "deno"]),
    ("cargo", &["cargo", "cargo-binstall"]),
    ("rubygems", &["gem", "bundler"]),
    ("cpan", &["cpanm", "cpan", "cpm"]),
    (
        "jvm",
        &[
            "coursier",
            "maven",
            "gradle",
            "sbt",
            "leiningen",
            "clojure",
            "ivy",
        ],
    ),
    ("nuget", &["dotnet", "paket"]),
    ("haskell", &["cabal", "stack"]),
    ("hex", &["mix", "rebar3"]),
];

/// Detected managers that share one package database. `managers` is sorted
/// by election preference; index 0 is the primary.
pub struct DatabaseGroup {
    pub database: &'static str,
    pub managers: Vec<Box<dyn PackageManager>>,
}

impl DatabaseGroup {
    pub fn primary(&self) -> &dyn PackageManager {
        self.managers[0].as_ref()
    }

    /// The member that should perform `operation`: the highest-preference
    /// one that supports it.
    pub fn elect(&self, operation: Operation) -> Option<&dyn PackageManager> {
        self.managers
            .iter()
            .map(|manager| manager.as_ref())
            .find(|manager| manager.supports(operation))
    }
}

/// Group managers by database and sort each group by [`PREFERENCE`]. The
/// sort is stable, so unlisted managers keep their registration order after
/// the listed ones.
pub fn group_by_database(managers: Vec<Box<dyn PackageManager>>) -> Vec<DatabaseGroup> {
    let mut groups: Vec<DatabaseGroup> = Vec::new();
    for manager in managers {
        let database = manager.database_id();
        match groups.iter_mut().find(|group| group.database == database) {
            Some(group) => group.managers.push(manager),
            None => groups.push(DatabaseGroup {
                database,
                managers: vec![manager],
            }),
        }
    }
    for group in &mut groups {
        group
            .managers
            .sort_by_key(|manager| rank(group.database, manager.id()));
    }
    groups.sort_by_key(|group| group.database);
    groups
}

fn rank(database: &str, id: &str) -> usize {
    PREFERENCE
        .iter()
        .find(|(entry, _)| *entry == database)
        .and_then(|(_, order)| order.iter().position(|candidate| *candidate == id))
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capabilities;
    use crate::error::Result;
    use crate::manager::{ManagerKind, OpContext};
    use crate::package::{Package, PackageRequest};

    #[derive(Debug)]
    struct Stub {
        id: &'static str,
        database: &'static str,
        capabilities: Capabilities,
    }

    #[async_trait::async_trait]
    impl PackageManager for Stub {
        fn id(&self) -> &'static str {
            self.id
        }
        fn display_name(&self) -> &'static str {
            self.id
        }
        fn kind(&self) -> ManagerKind {
            ManagerKind::System
        }
        fn database_id(&self) -> &'static str {
            self.database
        }
        fn capabilities(&self) -> Capabilities {
            self.capabilities
        }
        async fn install(&self, _: &[PackageRequest], _: &OpContext) -> Result<()> {
            Ok(())
        }
        async fn remove(&self, _: &[PackageRequest], _: &OpContext) -> Result<()> {
            Ok(())
        }
        async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
            Ok(Vec::new())
        }
        async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
            Err(crate::Error::NotFound(name.to_string()))
        }
    }

    fn stub(
        id: &'static str,
        database: &'static str,
        capabilities: Capabilities,
    ) -> Box<dyn PackageManager> {
        Box::new(Stub {
            id,
            database,
            capabilities,
        })
    }

    #[test]
    fn shared_database_forms_one_group_with_preferred_primary() {
        let groups = group_by_database(vec![
            stub("pacman", "alpm", Capabilities::CORE | Capabilities::REFRESH),
            stub("yay", "alpm", Capabilities::CORE | Capabilities::SEARCH),
            stub("flatpak", "flatpak", Capabilities::CORE),
        ]);
        assert_eq!(groups.len(), 2);
        let alpm = groups.iter().find(|g| g.database == "alpm").unwrap();
        assert_eq!(alpm.managers.len(), 2);
        assert_eq!(alpm.primary().id(), "yay");
    }

    #[test]
    fn election_falls_through_to_a_member_with_the_capability() {
        let groups = group_by_database(vec![
            stub("pacman", "alpm", Capabilities::CORE | Capabilities::REFRESH),
            stub("yay", "alpm", Capabilities::CORE | Capabilities::SEARCH),
        ]);
        let alpm = &groups[0];
        // yay is preferred overall, but only pacman can refresh here.
        assert_eq!(alpm.elect(Operation::Search).unwrap().id(), "yay");
        assert_eq!(alpm.elect(Operation::Refresh).unwrap().id(), "pacman");
        assert!(alpm.elect(Operation::ListOutdated).is_none());
    }
}
