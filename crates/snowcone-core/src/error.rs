use crate::capability::Operation;

/// Errors produced by snowcone core and its backends.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The backend does not implement this operation (see
    /// [`Capabilities`](crate::Capabilities)).
    #[error("`{manager}` does not support {operation}")]
    Unsupported {
        manager: String,
        operation: Operation,
    },

    /// A backend was requested by id but is not present on this host.
    #[error("backend `{0}` is not available on this host")]
    Unavailable(String),

    #[error("package `{0}` not found")]
    NotFound(String),

    /// Root was required but no elevation helper exists on the host.
    #[error("operation needs root and no elevation helper (sudo, doas, run0, pkexec) was found")]
    ElevationUnavailable,

    #[error("`{command}` failed ({status}): {stderr}")]
    CommandFailed {
        command: String,
        status: std::process::ExitStatus,
        stderr: String,
    },

    #[error("could not parse {what}: {detail}")]
    Parse { what: String, detail: String },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
