//! Subprocess plumbing shared by all backends: privilege elevation and
//! command execution with optional live output streaming.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;

use crate::error::{Error, Result};
use crate::host::{HostInfo, find_program};
use crate::progress::{EventSender, OutputStream, ProgressEvent};

/// How to reach root for backends that need it. Detected automatically —
/// never configured.
#[derive(Clone, Debug)]
pub enum Elevator {
    /// Already running as root.
    NotNeeded,
    /// Prefix commands with this helper.
    Helper(PathBuf),
    /// No way to elevate on this host; elevated commands fail with
    /// [`Error::ElevationUnavailable`].
    Unavailable,
}

impl Elevator {
    /// Checked in order; all four share the `helper cmd args…` calling shape.
    const HELPERS: [&'static str; 4] = ["sudo", "doas", "run0", "pkexec"];

    pub fn detect(host: &HostInfo) -> Self {
        if host.is_root {
            return Self::NotNeeded;
        }
        Self::HELPERS
            .iter()
            .find_map(|helper| find_program(helper))
            .map(Self::Helper)
            .unwrap_or(Self::Unavailable)
    }
}

/// Builder for backend subprocess invocations.
#[derive(Clone, Debug)]
pub struct Cmd {
    program: PathBuf,
    args: Vec<OsString>,
    elevate: bool,
}

impl Cmd {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            elevate: false,
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I>(mut self, args: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Run through the elevation helper (when not already root).
    pub fn elevated(mut self, elevate: bool) -> Self {
        self.elevate = elevate;
        self
    }

    /// Run with piped output, streaming each line to `events` when given.
    /// Use for anything that gets parsed or shown in the TUI.
    pub async fn capture(
        self,
        elevator: &Elevator,
        events: Option<&EventSender>,
    ) -> Result<CmdOutput> {
        let (mut command, rendered) = self.build(elevator)?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let (stdout, stderr) = tokio::join!(
            read_stream(stdout_pipe, OutputStream::Stdout, events),
            read_stream(stderr_pipe, OutputStream::Stderr, events),
        );
        let status = child.wait().await?;
        Ok(CmdOutput {
            command: rendered,
            status,
            stdout,
            stderr,
        })
    }

    /// Run attached to the user's terminal, so interactive prompts and
    /// native progress bars work. Output is not captured. Use for CLI
    /// passthrough of mutating operations.
    pub async fn run_interactive(self, elevator: &Elevator) -> Result<CmdOutput> {
        let (mut command, rendered) = self.build(elevator)?;
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let status = command.status().await?;
        Ok(CmdOutput {
            command: rendered,
            status,
            stdout: String::new(),
            stderr: String::new(),
        })
    }

    fn build(self, elevator: &Elevator) -> Result<(Command, String)> {
        let rendered = self.rendered();
        let (program, args) = if self.elevate {
            match elevator {
                Elevator::NotNeeded => (self.program, self.args),
                Elevator::Helper(helper) => {
                    let mut args = vec![self.program.into_os_string()];
                    args.extend(self.args);
                    (helper.clone(), args)
                }
                Elevator::Unavailable => return Err(Error::ElevationUnavailable),
            }
        } else {
            (self.program, self.args)
        };
        let mut command = Command::new(program);
        command.args(args);
        Ok((command, rendered))
    }

    fn rendered(&self) -> String {
        let mut rendered = self.program.display().to_string();
        for arg in &self.args {
            rendered.push(' ');
            rendered.push_str(&arg.to_string_lossy());
        }
        rendered
    }
}

/// Captured result of a finished command. Exit-code semantics vary per tool
/// (`dnf check-update` exits 100 to mean "updates exist"), so turning a
/// non-zero status into an error is opt-in via
/// [`CmdOutput::require_success`].
#[derive(Debug)]
pub struct CmdOutput {
    /// The command as invoked, for error messages.
    pub command: String,
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    pub fn success(&self) -> bool {
        self.status.success()
    }

    pub fn require_success(self) -> Result<Self> {
        if self.status.success() {
            Ok(self)
        } else {
            Err(Error::CommandFailed {
                command: self.command,
                status: self.status,
                stderr: self.stderr.trim().to_string(),
            })
        }
    }
}

async fn read_stream(
    pipe: Option<impl AsyncRead + Unpin>,
    stream: OutputStream,
    events: Option<&EventSender>,
) -> String {
    let Some(pipe) = pipe else {
        return String::new();
    };
    let mut lines = BufReader::new(pipe).lines();
    let mut collected = String::new();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(events) = events {
            let _ = events.send(ProgressEvent::Line {
                stream,
                text: line.clone(),
            });
        }
        collected.push_str(&line);
        collected.push('\n');
    }
    collected
}
