//! The operation set and per-backend capability flags.
//!
//! The interface is the broadest *common* subset of what every package
//! manager in the README can do. Only four operations are truly universal -
//! even `dpkg`, `rpm`, and Slackware's pkgtools can install, remove, list
//! what is installed, and show metadata. Everything past that (remote
//! search, index refresh, upgrade, outdated listing) is widespread but not
//! universal, so those operations are part of the interface too, but gated
//! behind [`Capabilities`] that each backend advertises.

use bitflags::bitflags;
use serde::Serialize;
use std::fmt;

/// Every operation `snow` can ask a backend to perform.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Operation {
    Install,
    Remove,
    ListInstalled,
    Info,
    Search,
    Refresh,
    Upgrade,
    ListOutdated,
}

impl Operation {
    /// Whether this operation changes package state (as opposed to reading
    /// it). Mutating operations are what confirmation prompts, elevation,
    /// and one-at-a-time scheduling care about.
    pub const fn mutates(self) -> bool {
        matches!(
            self,
            Operation::Install | Operation::Remove | Operation::Upgrade | Operation::Refresh
        )
    }

    /// The capability bit a backend must advertise to support this operation.
    pub const fn capability(self) -> Capabilities {
        match self {
            Operation::Install => Capabilities::INSTALL,
            Operation::Remove => Capabilities::REMOVE,
            Operation::ListInstalled => Capabilities::LIST_INSTALLED,
            Operation::Info => Capabilities::INFO,
            Operation::Search => Capabilities::SEARCH,
            Operation::Refresh => Capabilities::REFRESH,
            Operation::Upgrade => Capabilities::UPGRADE,
            Operation::ListOutdated => Capabilities::LIST_OUTDATED,
        }
    }
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Operation::Install => "install",
            Operation::Remove => "remove",
            Operation::ListInstalled => "list-installed",
            Operation::Info => "info",
            Operation::Search => "search",
            Operation::Refresh => "refresh",
            Operation::Upgrade => "upgrade",
            Operation::ListOutdated => "list-outdated",
        })
    }
}

bitflags! {
    /// What a backend can do. Capabilities are the single source of truth:
    /// a backend advertises exactly the operations that work, and election
    /// only routes an operation to a member advertising its bit. A tool
    /// that genuinely cannot perform one of the four core operations (bauh
    /// has no CLI verbs, zig's cache is opaque, makedeb only builds and
    /// installs) drops that bit and returns
    /// [`Error::Unsupported`](crate::Error::Unsupported) from the method.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Capabilities: u32 {
        const INSTALL = 1 << 0;
        const REMOVE = 1 << 1;
        const LIST_INSTALLED = 1 << 2;
        const INFO = 1 << 3;
        const SEARCH = 1 << 4;
        const REFRESH = 1 << 5;
        const UPGRADE = 1 << 6;
        const LIST_OUTDATED = 1 << 7;
        /// Can install a caller-chosen version rather than only the latest
        /// (apt `pkg=ver`, pip `pkg==ver`, npm `pkg@ver` - but not pacman).
        const PIN_VERSION = 1 << 8;
    }
}

impl Capabilities {
    /// The typical core set (install, remove, list-installed, info) -
    /// a convenience for the majority of backends, not a guarantee. A
    /// backend whose tool cannot perform one of these spells out the
    /// union of what actually works instead.
    pub const CORE: Self = Self::INSTALL
        .union(Self::REMOVE)
        .union(Self::LIST_INSTALLED)
        .union(Self::INFO);

    /// Human-readable names of the set bits, e.g. `["install", "search"]`.
    pub fn names(self) -> Vec<String> {
        self.iter_names()
            .map(|(name, _)| name.to_ascii_lowercase().replace('_', "-"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_is_the_universal_subset() {
        assert!(Capabilities::CORE.contains(Capabilities::INSTALL));
        assert!(Capabilities::CORE.contains(Capabilities::REMOVE));
        assert!(Capabilities::CORE.contains(Capabilities::LIST_INSTALLED));
        assert!(Capabilities::CORE.contains(Capabilities::INFO));
        assert!(!Capabilities::CORE.contains(Capabilities::SEARCH));
    }

    #[test]
    fn operations_map_to_their_bits() {
        assert_eq!(Operation::Search.capability(), Capabilities::SEARCH);
        assert_eq!(Operation::Install.capability(), Capabilities::INSTALL);
    }
}
