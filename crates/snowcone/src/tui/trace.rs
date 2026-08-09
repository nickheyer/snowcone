//! Tracing bridge for TUI mode: formatted log lines flow into the app's
//! message channel (rendered by the pinned "snowcone log" task) instead
//! of stderr, which would draw straight over the alternate screen.

use std::io;

use tokio::sync::mpsc::UnboundedSender;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

use super::app::TuiMsg;

pub struct ChannelWriter {
    tx: UnboundedSender<TuiMsg>,
    buffer: Vec<u8>,
}

impl io::Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(data);
        while let Some(newline) = self.buffer.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=newline).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1])
                .trim_end()
                .to_string();
            if !text.is_empty() {
                let _ = self.tx.send(TuiMsg::Log(text));
            }
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub struct MakeChannelWriter {
    tx: UnboundedSender<TuiMsg>,
}

impl<'a> MakeWriter<'a> for MakeChannelWriter {
    type Writer = ChannelWriter;

    fn make_writer(&'a self) -> Self::Writer {
        ChannelWriter {
            tx: self.tx.clone(),
            buffer: Vec::new(),
        }
    }
}

/// Install the bridge as the global subscriber. Best effort: if a
/// subscriber somehow exists already, logs keep flowing wherever they
/// were going.
pub fn init(tx: UnboundedSender<TuiMsg>) {
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(MakeChannelWriter { tx })
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
}
