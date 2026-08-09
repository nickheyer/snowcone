//! Terminal input, bridged from a crossterm polling thread onto a channel
//! the async app loop can `select!` on - with a synchronous pause
//! handshake so the TUI can hand the terminal to an interactive child
//! without this thread racing it for keystrokes.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

#[derive(Clone, Copy, PartialEq, Eq)]
enum PauseState {
    Running,
    PauseRequested,
    Paused,
    Shutdown,
}

struct Shared {
    state: Mutex<PauseState>,
    condvar: Condvar,
}

pub struct InputReader {
    rx: mpsc::UnboundedReceiver<crossterm::event::Event>,
    shared: Arc<Shared>,
}

impl InputReader {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            state: Mutex::new(PauseState::Running),
            condvar: Condvar::new(),
        });
        let thread_shared = Arc::clone(&shared);
        std::thread::spawn(move || reader_loop(tx, thread_shared));
        Self { rx, shared }
    }

    pub async fn recv(&mut self) -> Option<crossterm::event::Event> {
        self.rx.recv().await
    }

    /// Block until the reader thread acknowledges it is parked (at most
    /// one poll timeout away). Only after this returns is it safe to give
    /// the terminal to a child process. Call via `block_in_place`.
    ///
    /// A keystroke that slips in before the ack was typed before the
    /// suspension and lands in the channel - processed after resume,
    /// which is correct; it can never be stolen from the child, because
    /// the thread is provably parked before the child exists.
    pub fn pause(&self) {
        let mut state = self.shared.state.lock().unwrap();
        if *state == PauseState::Shutdown {
            return;
        }
        *state = PauseState::PauseRequested;
        while *state == PauseState::PauseRequested {
            state = self.shared.condvar.wait(state).unwrap();
        }
    }

    pub fn resume(&self) {
        let mut state = self.shared.state.lock().unwrap();
        if *state != PauseState::Shutdown {
            *state = PauseState::Running;
        }
        self.shared.condvar.notify_all();
    }
}

impl Drop for InputReader {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap();
        *state = PauseState::Shutdown;
        self.shared.condvar.notify_all();
    }
}

fn reader_loop(
    tx: mpsc::UnboundedSender<crossterm::event::Event>,
    shared: Arc<Shared>,
) {
    loop {
        {
            let mut state = shared.state.lock().unwrap();
            loop {
                match *state {
                    PauseState::Shutdown => return,
                    PauseState::PauseRequested => {
                        *state = PauseState::Paused;
                        shared.condvar.notify_all();
                    }
                    PauseState::Paused => state = shared.condvar.wait(state).unwrap(),
                    PauseState::Running => break,
                }
            }
        }
        // Short poll so a pause request is honored within ~100ms even
        // when no keys arrive.
        match crossterm::event::poll(Duration::from_millis(100)) {
            Ok(true) => match crossterm::event::read() {
                Ok(event) => {
                    if tx.send(event).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            },
            Ok(false) => {}
            Err(_) => return,
        }
    }
}
