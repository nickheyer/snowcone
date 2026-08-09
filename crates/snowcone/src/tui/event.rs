//! Terminal input, bridged from crossterm's blocking reader onto a channel
//! the async app loop can `select!` on.

use tokio::sync::mpsc;

pub fn input_channel() -> mpsc::UnboundedReceiver<crossterm::event::Event> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            if tx.send(event).is_err() {
                break;
            }
        }
    });
    rx
}
