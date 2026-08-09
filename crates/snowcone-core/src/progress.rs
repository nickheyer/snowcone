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
    /// A raw line of subprocess output.
    Line { stream: OutputStream, text: String },
    /// Determinate progress, when the backend can tell.
    Progress { current: u64, total: u64 },
}

pub type EventSender = mpsc::UnboundedSender<ProgressEvent>;
pub type EventReceiver = mpsc::UnboundedReceiver<ProgressEvent>;

pub fn progress_channel() -> (EventSender, EventReceiver) {
    mpsc::unbounded_channel()
}
