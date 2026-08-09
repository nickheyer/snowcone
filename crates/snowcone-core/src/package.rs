//! The package interface: the broadest common subset of metadata the
//! ecosystem can describe a package with.
//!
//! Only `name` is truly universal, and a version is almost always known, so
//! those anchor the trait. Everything else (description, homepage, license,
//! sizes, dependencies, …) is common but not guaranteed — those accessors
//! default to `None` and backends override what their format actually
//! carries.

use serde::Serialize;
use std::fmt;

/// How a package relates to the local system.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallState {
    Installed,
    /// Installed, and a newer version is available.
    Upgradable,
    /// Known to a repository but not installed.
    Available,
    #[default]
    Unknown,
}

impl fmt::Display for InstallState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            InstallState::Installed => "installed",
            InstallState::Upgradable => "upgradable",
            InstallState::Available => "available",
            InstallState::Unknown => "unknown",
        })
    }
}

/// A package as one backend sees it. Each backend crate provides its own
/// implementing type (e.g. `AptPackage`, `CargoPackage`).
pub trait Package: fmt::Debug + Send + Sync {
    /// Id of the backend this package came from (e.g. `"apt"`).
    fn manager(&self) -> &str;

    fn name(&self) -> &str;

    /// Installed or candidate version, whichever the backend was describing.
    fn version(&self) -> Option<&str>;

    /// Newest known version, when it differs from [`Package::version`]
    /// (outdated listings).
    fn latest_version(&self) -> Option<&str> {
        None
    }

    fn description(&self) -> Option<&str> {
        None
    }

    fn homepage(&self) -> Option<&str> {
        None
    }

    fn license(&self) -> Option<&str> {
        None
    }

    fn architecture(&self) -> Option<&str> {
        None
    }

    /// Where the package comes from: repository, registry, channel, remote…
    fn origin(&self) -> Option<&str> {
        None
    }

    fn installed_size(&self) -> Option<u64> {
        None
    }

    fn download_size(&self) -> Option<u64> {
        None
    }

    /// Direct runtime dependencies, as backend-native names.
    fn dependencies(&self) -> Option<Vec<String>> {
        None
    }

    fn state(&self) -> InstallState {
        InstallState::Unknown
    }
}

/// Owned, serializable snapshot of any [`Package`] — what the CLI prints and
/// the TUI renders.
#[derive(Clone, Debug, Serialize)]
pub struct PackageSummary {
    pub manager: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl PackageSummary {
    pub fn new(package: &dyn Package) -> Self {
        Self {
            manager: package.manager().to_owned(),
            name: package.name().to_owned(),
            version: package.version().map(str::to_owned),
            latest_version: package.latest_version().map(str::to_owned),
            description: package.description().map(str::to_owned),
            homepage: package.homepage().map(str::to_owned),
            license: package.license().map(str::to_owned),
            architecture: package.architecture().map(str::to_owned),
            origin: package.origin().map(str::to_owned),
            installed_size: package.installed_size(),
            download_size: package.download_size(),
            dependencies: package.dependencies(),
            state: package.state(),
        }
    }
}

/// A package as requested on the command line: `ripgrep` or
/// `ripgrep@14.1.0`. Backends translate the optional version into their own
/// pinning syntax, or reject it if they lack
/// [`Capabilities::PIN_VERSION`](crate::Capabilities::PIN_VERSION).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageRequest {
    pub name: String,
    pub version: Option<String>,
}

impl PackageRequest {
    /// Split `name@version`; a leading `@` (npm scopes) belongs to the name.
    pub fn parse(spec: &str) -> Self {
        let at = spec
            .char_indices()
            .skip(1)
            .find(|&(_, c)| c == '@')
            .map(|(i, _)| i);
        match at {
            Some(i) => Self {
                name: spec[..i].to_string(),
                version: Some(spec[i + 1..].to_string()).filter(|v| !v.is_empty()),
            },
            None => Self {
                name: spec.to_string(),
                version: None,
            },
        }
    }
}

impl fmt::Display for PackageRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.version {
            Some(version) => write!(f, "{}@{}", self.name, version),
            None => f.write_str(&self.name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_name() {
        let req = PackageRequest::parse("ripgrep");
        assert_eq!(req.name, "ripgrep");
        assert_eq!(req.version, None);
    }

    #[test]
    fn parses_name_with_version() {
        let req = PackageRequest::parse("ripgrep@14.1.0");
        assert_eq!(req.name, "ripgrep");
        assert_eq!(req.version.as_deref(), Some("14.1.0"));
    }

    #[test]
    fn npm_scope_stays_in_the_name() {
        let req = PackageRequest::parse("@types/node@22.0.0");
        assert_eq!(req.name, "@types/node");
        assert_eq!(req.version.as_deref(), Some("22.0.0"));
    }
}
