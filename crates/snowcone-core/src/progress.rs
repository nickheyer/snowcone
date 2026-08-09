//! Progress reporting from backend operations to the TUI/CLI.

use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// Events a backend emits while an operation runs.
#[derive(Clone, Debug)]
pub enum ProgressEvent {
    /// Backend status message ("resolving dependencies…").
    Status(String),
    /// A raw line of subprocess output. `transient` lines were terminated
    /// by a bare carriage return - the tool meant to overwrite them (progress
    /// bars), so renderers should replace the previous transient line rather
    /// than append.
    Line {
        stream: OutputStream,
        text: String,
        transient: bool,
    },
    /// Determinate progress, when the backend can tell.
    Progress { current: u64, total: u64 },
}

pub type EventSender = mpsc::UnboundedSender<ProgressEvent>;
pub type EventReceiver = mpsc::UnboundedReceiver<ProgressEvent>;

pub fn progress_channel() -> (EventSender, EventReceiver) {
    mpsc::unbounded_channel()
}
