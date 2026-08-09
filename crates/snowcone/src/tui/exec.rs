//! Mutation execution: captured (streamed into the Tasks pane) and
//! interactive (TUI suspended, tool owns the terminal).
//!
//! Both paths run the manager the user confirmed - found by id in the
//! group snapshot the plan was made against, never re-elected.

use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{cursor, execute};
use ratatui::DefaultTerminal;
use snowcone_core::{
    DatabaseGroup, Error, OpContext, Operation, OutputStream, PackageManager, ProgressEvent,
    progress_channel,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use super::app::TuiMsg;
use super::event::InputReader;
use super::pool::MutationPlan;
use super::tasks::{LineSource, OutputLine, TaskId};

/// Run a captured mutation in the background: piped output streams into
/// the Tasks pane, `assume_yes` forced (the TUI confirm modal was the
/// confirmation), `dry_run` never.
pub fn spawn_captured(
    plan: MutationPlan,
    groups: Arc<Vec<DatabaseGroup>>,
    task: TaskId,
    tx: UnboundedSender<TuiMsg>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = run_captured(&groups, &plan, task, &tx)
            .await
            .map_err(|error| error.to_string());
        let _ = tx.send(TuiMsg::TaskDone { id: task, result });
    })
}

async fn run_captured(
    groups: &[DatabaseGroup],
    plan: &MutationPlan,
    task: TaskId,
    tx: &UnboundedSender<TuiMsg>,
) -> snowcone_core::Result<()> {
    let manager = find_manager(groups, plan)?;
    let (event_tx, mut event_rx) = progress_channel();
    let ctx = OpContext {
        assume_yes: true,
        dry_run: false,
        events: Some(event_tx),
    };
    let result = {
        let mut op = std::pin::pin!(dispatch(manager, plan.operation, plan, &ctx));
        loop {
            tokio::select! {
                event = event_rx.recv() => {
                    if let Some(event) = event {
                        forward(tx, task, event);
                    }
                }
                result = &mut op => break result,
            }
        }
    };
    // The op future is done; close our sender and flush whatever the
    // select loop hadn't forwarded yet.
    drop(ctx);
    while let Ok(event) = event_rx.try_recv() {
        forward(tx, task, event);
    }
    result
}

/// Suspend the TUI and run the plan on the real terminal: sudo prompts,
/// native progress bars, tool-level questions all work. Blocks the App
/// loop for the duration - that is the point; the terminal belongs to
/// the child.
pub async fn run_suspended(
    terminal: &mut DefaultTerminal,
    input: &InputReader,
    groups: &[DatabaseGroup],
    plan: &MutationPlan,
) -> Result<(), String> {
    // The reader thread is provably parked before the child exists, so it
    // can never steal the child's keystrokes.
    tokio::task::block_in_place(|| input.pause());
    let result = match leave_tui() {
        Ok(()) => {
            banner(plan);
            let outcome = match find_manager(groups, plan) {
                Ok(manager) => {
                    let ctx = OpContext {
                        assume_yes: false,
                        dry_run: false,
                        events: None,
                    };
                    dispatch(manager, plan.operation, plan, &ctx)
                        .await
                        .map_err(|error| error.to_string())
                }
                Err(error) => Err(error.to_string()),
            };
            match &outcome {
                Ok(()) => println!("snow: done"),
                Err(error) => println!("snow: failed: {error}"),
            }
            wait_for_enter().await;
            outcome
        }
        Err(error) => Err(format!("could not release the terminal: {error}")),
    };
    // Every exit path from here restores the TUI and the reader thread.
    reenter_tui(terminal);
    input.resume();
    result
}

fn find_manager<'a>(
    groups: &'a [DatabaseGroup],
    plan: &MutationPlan,
) -> snowcone_core::Result<&'a dyn PackageManager> {
    groups
        .iter()
        .find(|group| group.database == plan.database)
        .and_then(|group| {
            group
                .managers
                .iter()
                .map(|manager| manager.as_ref())
                .find(|manager| manager.id() == plan.manager_id)
        })
        .ok_or_else(|| {
            Error::Other(format!(
                "{} [{}] is no longer available",
                plan.manager_id, plan.database
            ))
        })
}

async fn dispatch(
    manager: &dyn PackageManager,
    operation: Operation,
    plan: &MutationPlan,
    ctx: &OpContext,
) -> snowcone_core::Result<()> {
    match operation {
        Operation::Install => manager.install(&plan.requests, ctx).await,
        Operation::Remove => manager.remove(&plan.requests, ctx).await,
        Operation::Upgrade => manager.upgrade(&plan.requests, ctx).await,
        Operation::Refresh => manager.refresh(ctx).await,
        other => Err(Error::Other(format!("{other} is not a mutation"))),
    }
}

fn forward(tx: &UnboundedSender<TuiMsg>, task: TaskId, event: ProgressEvent) {
    let msg = match event {
        ProgressEvent::Line {
            stream,
            text,
            transient,
        } => TuiMsg::TaskOutput {
            id: task,
            line: OutputLine {
                source: match stream {
                    OutputStream::Stdout => LineSource::Stdout,
                    OutputStream::Stderr => LineSource::Stderr,
                },
                text,
                transient,
            },
        },
        ProgressEvent::Status(text) => TuiMsg::TaskOutput {
            id: task,
            line: OutputLine {
                source: LineSource::Status,
                text,
                transient: false,
            },
        },
        ProgressEvent::Progress { current, total } => TuiMsg::TaskProgress {
            id: task,
            current,
            total,
        },
    };
    let _ = tx.send(msg);
}

fn leave_tui() -> std::io::Result<()> {
    disable_raw_mode()?;
    execute!(std::io::stdout(), LeaveAlternateScreen, cursor::Show)?;
    Ok(())
}

fn reenter_tui(terminal: &mut DefaultTerminal) {
    let _ = enable_raw_mode();
    let _ = execute!(std::io::stdout(), EnterAlternateScreen);
    // Drop type-ahead typed at the "press Enter" prompt so it doesn't
    // fire actions in the TUI.
    while let Ok(true) = crossterm::event::poll(Duration::ZERO) {
        let _ = crossterm::event::read();
    }
    // The back buffer no longer matches the screen; force a full repaint.
    let _ = terminal.clear();
}

fn banner(plan: &MutationPlan) {
    println!();
    println!("snow: running: {}", plan.title);
    if plan.needs_elevation {
        println!("snow: this may prompt for credentials.");
    }
    println!("snow: Ctrl-C aborts the tool as usual.");
    println!();
}

async fn wait_for_enter() {
    print!("snow: press Enter to return to the TUI… ");
    let _ = std::io::stdout().flush();
    // Cooked-mode line read on a blocking thread: the paused reader
    // thread and raw-mode crossterm are both uninvolved.
    let _ = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    })
    .await;
}
