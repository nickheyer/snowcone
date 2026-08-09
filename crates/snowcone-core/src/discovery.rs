//! Automatic backend discovery.
//!
//! Every backend crate exposes a [`BackendFactory`]; the `snow` binary
//! registers them all in a [`Registry`] at startup. Discovery probes the
//! host (executables, os-release) with zero user configuration — a backend
//! either detects itself or stays out of the way.

use std::path::PathBuf;

use crate::error::Result;
use crate::host::HostInfo;
use crate::manager::PackageManager;

/// Result of probing one backend on this host.
#[derive(Clone, Debug)]
pub enum Detection {
    /// The backend's CLI was found and looks usable.
    Available { program: PathBuf },
    /// Not on this host (with a human-readable why, for `snow managers`).
    Unavailable { reason: String },
}

impl Detection {
    pub fn is_available(&self) -> bool {
        matches!(self, Detection::Available { .. })
    }
}

/// One per backend crate: knows how to detect and construct its manager.
pub trait BackendFactory: Send + Sync {
    /// Must match the constructed manager's
    /// [`PackageManager::id`](crate::PackageManager::id).
    fn id(&self) -> &'static str;

    /// Probe the host. Must be side-effect free and must not depend on any
    /// user configuration.
    fn detect(&self, host: &HostInfo) -> Detection;

    fn create(&self, host: &HostInfo) -> Result<Box<dyn PackageManager>>;
}

/// A probed factory, for status displays.
pub struct Probe<'a> {
    pub factory: &'a dyn BackendFactory,
    pub detection: Detection,
}

/// All registered backends. The `snow` binary builds one of these at
/// startup; backend crates get registered there and nowhere else.
#[derive(Default)]
pub struct Registry {
    factories: Vec<Box<dyn BackendFactory>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, factory: Box<dyn BackendFactory>) {
        self.factories.push(factory);
    }

    pub fn factories(&self) -> &[Box<dyn BackendFactory>] {
        &self.factories
    }

    /// Probe every registered backend, available or not.
    pub fn probe(&self, host: &HostInfo) -> Vec<Probe<'_>> {
        self.factories
            .iter()
            .map(|factory| Probe {
                factory: factory.as_ref(),
                detection: factory.detect(host),
            })
            .collect()
    }

    /// Instantiate every backend that detects as available.
    pub fn discover(&self, host: &HostInfo) -> Vec<Box<dyn PackageManager>> {
        let mut managers = Vec::new();
        for probe in self.probe(host) {
            if !probe.detection.is_available() {
                continue;
            }
            match probe.factory.create(host) {
                Ok(manager) => managers.push(manager),
                Err(error) => tracing::warn!(
                    backend = probe.factory.id(),
                    %error,
                    "backend detected but failed to initialize",
                ),
            }
        }
        managers
    }
}
