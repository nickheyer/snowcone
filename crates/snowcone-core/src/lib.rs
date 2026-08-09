//! Core plumbing for snowcone: the [`PackageManager`] and [`Package`]
//! interfaces, backend discovery, host introspection, privilege elevation,
//! and subprocess execution.
//!
//! Backend crates (one per package manager) implement [`PackageManager`] and
//! [`Package`] and expose a [`BackendFactory`] that the `snow` binary
//! registers at startup. Discovery is fully automatic: factories probe the
//! host (binaries on `PATH`, os-release) and never rely on user
//! configuration.

pub mod capability;
pub mod discovery;
pub mod election;
pub mod error;
pub mod exec;
pub mod host;
pub mod manager;
pub mod package;
pub mod progress;

pub use capability::{Capabilities, Operation};
pub use discovery::{BackendFactory, Detection, Probe, Registry};
pub use election::{DatabaseGroup, PREFERENCE, group_by_database};
pub use error::{Error, Result};
pub use exec::{Cmd, CmdOutput, Elevator};
pub use host::{HostInfo, OsRelease, find_program};
pub use manager::{ManagerKind, OpContext, PackageManager};
pub use package::{InstallState, Package, PackageRequest, PackageSummary};
pub use progress::{EventReceiver, EventSender, OutputStream, ProgressEvent, progress_channel};
