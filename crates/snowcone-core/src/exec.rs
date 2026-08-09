//! Subprocess plumbing shared by all backends: privilege elevation and
//! command execution with optional live output streaming.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::error::{Error, Result};
use crate::host::{HostInfo, find_program};
use crate::progress::{EventSender, OutputStream, ProgressEvent};

/// How to reach root for backends that need it. Detected automatically -
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
    envs: Vec<(OsString, OsString)>,
    elevate: bool,
}

impl Cmd {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            envs: Vec::new(),
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

    /// Set an environment variable for the child (`LC_ALL=C` keeps output
    /// parseable regardless of the user's locale).
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.envs.push((key.into(), value.into()));
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
    ///
    /// Deliberately no `kill_on_drop`: this future only drops mid-run
    /// during whole-process teardown, and SIGKILLing sudo mid-transaction
    /// is worse than letting the child finish - sudo can't relay SIGKILL,
    /// so the elevated grandchild would survive orphaned mid-write anyway.
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
        // Rendered after elevation resolves, so error messages show the
        // real invocation (`/usr/bin/sudo apt install …`, not `apt …`).
        let rendered = render(&program, &args);
        let mut command = Command::new(program);
        command.args(args);
        command.envs(self.envs);
        Ok((command, rendered))
    }
}

fn render(program: &Path, args: &[OsString]) -> String {
    let mut rendered = program.display().to_string();
    for arg in args {
        rendered.push(' ');
        rendered.push_str(&arg.to_string_lossy());
    }
    rendered
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

/// Drain a pipe, forwarding each line to `events` as it completes and
/// returning everything read, lossily decoded.
///
/// Byte-level scanner rather than `BufReader::lines()` because tools draw
/// progress bars with bare `\r` - those frames must stream out as they
/// happen (flagged transient), not arrive as one giant line at EOF. Lines
/// are decoded only once complete, so a multibyte char split across read
/// boundaries never turns into U+FFFD; invalid UTF-8 degrades to U+FFFD
/// instead of silently ending the stream. Read errors mean the pipe is
/// gone (child died mid-write) and are treated as EOF, keeping what was
/// already read.
async fn read_stream(
    pipe: Option<impl AsyncRead + Unpin>,
    stream: OutputStream,
    events: Option<&EventSender>,
) -> String {
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let mut collected: Vec<u8> = Vec::new();
    let mut segment: Vec<u8> = Vec::new();
    let mut pending_cr = false;
    let mut chunk = [0u8; 8192];
    loop {
        let count = match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        let bytes = &chunk[..count];
        collected.extend_from_slice(bytes);
        for &byte in bytes {
            if pending_cr {
                pending_cr = false;
                // \r\n is one ordinary break; a bare \r ends a transient
                // (meant-to-be-overwritten) line.
                emit_line(events, stream, &mut segment, byte != b'\n');
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\n' => emit_line(events, stream, &mut segment, false),
                // The next byte decides between \r\n and a bare \r, and it
                // may sit in the next chunk - defer.
                b'\r' => pending_cr = true,
                _ => segment.push(byte),
            }
        }
    }
    if pending_cr {
        emit_line(events, stream, &mut segment, true);
    } else if !segment.is_empty() {
        emit_line(events, stream, &mut segment, false);
    }
    String::from_utf8_lossy(&collected).into_owned()
}

fn emit_line(
    events: Option<&EventSender>,
    stream: OutputStream,
    segment: &mut Vec<u8>,
    transient: bool,
) {
    if let Some(events) = events {
        let _ = events.send(ProgressEvent::Line {
            stream,
            text: String::from_utf8_lossy(segment).into_owned(),
            transient,
        });
    }
    segment.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::progress_channel;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::ReadBuf;

    /// Feeds fixed-size chunks so breaks and multibyte chars can be forced
    /// to straddle read boundaries.
    struct ChunkReader {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
    }

    impl ChunkReader {
        fn new(data: impl Into<Vec<u8>>, chunk: usize) -> Self {
            Self {
                data: data.into(),
                pos: 0,
                chunk,
            }
        }
    }

    impl AsyncRead for ChunkReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let take = self
                .chunk
                .min(self.data.len() - self.pos)
                .min(buf.remaining());
            if take > 0 {
                let start = self.pos;
                buf.put_slice(&self.data[start..start + take]);
                self.pos += take;
            }
            Poll::Ready(Ok(()))
        }
    }

    async fn scan(data: impl Into<Vec<u8>>, chunk: usize) -> (String, Vec<(String, bool)>) {
        let (tx, mut rx) = progress_channel();
        let collected = read_stream(
            Some(ChunkReader::new(data, chunk)),
            OutputStream::Stdout,
            Some(&tx),
        )
        .await;
        drop(tx);
        let mut lines = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let ProgressEvent::Line {
                text, transient, ..
            } = event
            {
                lines.push((text, transient));
            }
        }
        (collected, lines)
    }

    fn line(text: &str, transient: bool) -> (String, bool) {
        (text.to_string(), transient)
    }

    #[tokio::test]
    async fn lf_lines() {
        let (collected, lines) = scan("one\ntwo\n", 3).await;
        assert_eq!(collected, "one\ntwo\n");
        assert_eq!(lines, vec![line("one", false), line("two", false)]);
    }

    #[tokio::test]
    async fn crlf_is_one_break() {
        let (collected, lines) = scan("a\r\nb\r\n", 4).await;
        assert_eq!(collected, "a\r\nb\r\n");
        assert_eq!(lines, vec![line("a", false), line("b", false)]);
    }

    #[tokio::test]
    async fn bare_cr_frames_are_transient() {
        let (_, lines) = scan("10%\r50%\r100%\n", 5).await;
        assert_eq!(
            lines,
            vec![line("10%", true), line("50%", true), line("100%", false)]
        );
    }

    #[tokio::test]
    async fn crlf_split_across_chunks() {
        // Chunk size 3 puts the \r at the end of the first read.
        let (_, lines) = scan("ab\r\ncd", 3).await;
        assert_eq!(lines, vec![line("ab", false), line("cd", false)]);
    }

    #[tokio::test]
    async fn multibyte_split_across_chunks() {
        let (collected, lines) = scan("héllo\n", 1).await;
        assert_eq!(collected, "héllo\n");
        assert_eq!(lines, vec![line("héllo", false)]);
    }

    #[tokio::test]
    async fn invalid_utf8_degrades_lossily() {
        let (collected, lines) = scan(b"a\xffb\nrest\n".to_vec(), 4).await;
        assert_eq!(collected, "a\u{fffd}b\nrest\n");
        assert_eq!(lines, vec![line("a\u{fffd}b", false), line("rest", false)]);
    }

    #[tokio::test]
    async fn lone_cr_at_eof_is_transient() {
        let (collected, lines) = scan("spinning\r", 8192).await;
        assert_eq!(collected, "spinning\r");
        assert_eq!(lines, vec![line("spinning", true)]);
    }

    #[tokio::test]
    async fn unterminated_tail_flushes() {
        let (_, lines) = scan("partial", 8192).await;
        assert_eq!(lines, vec![line("partial", false)]);
    }

    #[test]
    fn rendered_command_includes_elevation_helper() {
        let cmd = Cmd::new("apt")
            .args(["install", "-y", "ripgrep"])
            .elevated(true);
        let elevator = Elevator::Helper(PathBuf::from("/usr/bin/sudo"));
        let (_, rendered) = cmd.build(&elevator).unwrap();
        assert_eq!(rendered, "/usr/bin/sudo apt install -y ripgrep");
    }

    #[test]
    fn rendered_command_without_elevation() {
        let (_, rendered) = Cmd::new("npm")
            .arg("install")
            .build(&Elevator::Unavailable)
            .unwrap();
        assert_eq!(rendered, "npm install");
    }

    #[test]
    fn elevation_unavailable_fails() {
        let error = Cmd::new("apt")
            .elevated(true)
            .build(&Elevator::Unavailable)
            .unwrap_err();
        assert!(matches!(error, Error::ElevationUnavailable));
    }
}
