//! Task registry: every operation the TUI runs - reads and mutations -
//! as plain data owned by the App loop. Background tasks never touch it;
//! they report through `TuiMsg` and the App updates the registry.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use snowcone_core::Operation;
use tokio::task::AbortHandle;

use super::policy::ExecMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TaskId(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskKind {
    Search,
    List,
    Info,
    Install,
    Remove,
    Upgrade,
    Refresh,
}

impl TaskKind {
    pub fn from_operation(operation: Operation) -> Self {
        match operation {
            Operation::Install => TaskKind::Install,
            Operation::Remove => TaskKind::Remove,
            Operation::Upgrade => TaskKind::Upgrade,
            Operation::Refresh => TaskKind::Refresh,
            Operation::Search => TaskKind::Search,
            Operation::Info => TaskKind::Info,
            Operation::ListInstalled | Operation::ListOutdated => TaskKind::List,
        }
    }

    pub fn is_mutation(self) -> bool {
        matches!(
            self,
            TaskKind::Install | TaskKind::Remove | TaskKind::Upgrade | TaskKind::Refresh
        )
    }

}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Succeeded,
    Failed(String),
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineSource {
    Stdout,
    Stderr,
    Status,
}

#[derive(Clone, Debug)]
pub struct OutputLine {
    pub source: LineSource,
    pub text: String,
    /// Progress-bar frame: the next line on this task replaces it.
    pub transient: bool,
}

/// Per-task output ring capacity.
pub const OUTPUT_CAP: usize = 2000;
/// Finished tasks kept around for the Tasks tab history.
pub const FINISHED_RETAIN: usize = 20;

pub struct Task {
    pub id: TaskId,
    pub kind: TaskKind,
    pub title: String,
    pub manager: Option<String>,
    pub database: Option<&'static str>,
    pub mode: Option<ExecMode>,
    /// Package names a mutation targets, for optimistic state flips.
    pub names: Vec<String>,
    pub status: TaskStatus,
    pub started: Instant,
    pub finished: Option<Instant>,
    pub progress: Option<(u64, u64)>,
    pub output: VecDeque<OutputLine>,
    abort: Option<AbortHandle>,
}

impl Task {
    pub fn running(&self) -> bool {
        self.status == TaskStatus::Running
    }

    pub fn elapsed(&self) -> Duration {
        match self.finished {
            Some(finished) => finished.duration_since(self.started),
            None => self.started.elapsed(),
        }
    }
}

pub struct TaskRegistry {
    next_id: u64,
    tasks: Vec<Task>,
    /// The one mutation allowed to run at a time (package databases lock).
    pub active_mutation: Option<TaskId>,
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            tasks: Vec::new(),
            active_mutation: None,
        }
    }

    pub fn begin(&mut self, kind: TaskKind, title: String) -> TaskId {
        let id = TaskId(self.next_id);
        self.next_id += 1;
        self.tasks.push(Task {
            id,
            kind,
            title,
            manager: None,
            database: None,
            mode: None,
            names: Vec::new(),
            status: TaskStatus::Running,
            started: Instant::now(),
            finished: None,
            progress: None,
            output: VecDeque::new(),
            abort: None,
        });
        self.prune();
        id
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    pub fn get(&self, id: TaskId) -> Option<&Task> {
        self.tasks.iter().find(|task| task.id == id)
    }

    pub fn get_mut(&mut self, id: TaskId) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|task| task.id == id)
    }

    pub fn set_abort(&mut self, id: TaskId, handle: AbortHandle) {
        if let Some(task) = self.get_mut(id) {
            task.abort = Some(handle);
        }
    }

    /// Append a line, coalescing progress-bar frames: a transient last
    /// line is replaced rather than appended to. A bare empty line right
    /// after a transient one just finalizes it (some tools end a progress
    /// bar with `\r\n`).
    pub fn push_output(&mut self, id: TaskId, line: OutputLine) {
        let Some(task) = self.get_mut(id) else {
            return;
        };
        if let Some(last) = task.output.back_mut()
            && last.transient
        {
            if line.text.is_empty() && !line.transient {
                last.transient = false;
                return;
            }
            *last = line;
            return;
        }
        task.output.push_back(line);
        if task.output.len() > OUTPUT_CAP {
            task.output.pop_front();
        }
    }

    pub fn set_progress(&mut self, id: TaskId, current: u64, total: u64) {
        if let Some(task) = self.get_mut(id) {
            task.progress = Some((current, total));
        }
    }

    /// Terminal states are sticky: a `TaskDone` racing a cancellation is
    /// ignored.
    pub fn finish(&mut self, id: TaskId, result: Result<(), String>) {
        if self.active_mutation == Some(id) {
            self.active_mutation = None;
        }
        if let Some(task) = self.get_mut(id)
            && task.status == TaskStatus::Running
        {
            task.status = match result {
                Ok(()) => TaskStatus::Succeeded,
                Err(error) => TaskStatus::Failed(error),
            };
            task.finished = Some(Instant::now());
            task.abort = None;
        }
    }

    /// Abort a running task. Capture children die with the future
    /// (`kill_on_drop`); interactive tasks have no handle and return
    /// false - the user owns the terminal there.
    pub fn cancel(&mut self, id: TaskId) -> bool {
        if self.active_mutation == Some(id) {
            self.active_mutation = None;
        }
        let Some(task) = self.get_mut(id) else {
            return false;
        };
        if !task.running() {
            return false;
        }
        let Some(abort) = task.abort.take() else {
            return false;
        };
        abort.abort();
        task.status = TaskStatus::Cancelled;
        task.finished = Some(Instant::now());
        true
    }

    /// Quit path: kill everything that can be killed.
    pub fn abort_all(&mut self) {
        let ids: Vec<TaskId> = self
            .tasks
            .iter()
            .filter(|task| task.running())
            .map(|task| task.id)
            .collect();
        for id in ids {
            self.cancel(id);
        }
    }

    pub fn clear_finished(&mut self) {
        self.tasks.retain(|task| task.running());
    }

    pub fn running_count(&self) -> usize {
        self.tasks.iter().filter(|task| task.running()).count()
    }

    fn prune(&mut self) {
        let finished = self.tasks.iter().filter(|task| !task.running()).count();
        if finished <= FINISHED_RETAIN {
            return;
        }
        let mut to_drop = finished - FINISHED_RETAIN;
        self.tasks.retain(|task| {
            if to_drop > 0 && !task.running() {
                to_drop -= 1;
                false
            } else {
                true
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, transient: bool) -> OutputLine {
        OutputLine {
            source: LineSource::Stdout,
            text: text.to_string(),
            transient,
        }
    }

    #[test]
    fn transient_lines_coalesce() {
        let mut registry = TaskRegistry::new();
        let id = registry.begin(TaskKind::Install, "install x".to_string());
        registry.push_output(id, line("10%", true));
        registry.push_output(id, line("50%", true));
        registry.push_output(id, line("100%", false));
        registry.push_output(id, line("done", false));
        let task = registry.get(id).unwrap();
        let texts: Vec<&str> = task.output.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["100%", "done"]);
    }

    #[test]
    fn empty_line_finalizes_a_transient_frame() {
        let mut registry = TaskRegistry::new();
        let id = registry.begin(TaskKind::Install, "install x".to_string());
        registry.push_output(id, line("downloading… 100%", true));
        registry.push_output(id, line("", false));
        let task = registry.get(id).unwrap();
        assert_eq!(task.output.len(), 1);
        assert_eq!(task.output[0].text, "downloading… 100%");
        assert!(!task.output[0].transient);
    }

    #[test]
    fn terminal_states_are_sticky() {
        let mut registry = TaskRegistry::new();
        let id = registry.begin(TaskKind::Search, "search".to_string());
        // No abort handle set: cancel refuses…
        assert!(!registry.cancel(id));
        registry.finish(id, Ok(()));
        // …and a second finish can't overwrite.
        registry.finish(id, Err("late".to_string()));
        assert_eq!(registry.get(id).unwrap().status, TaskStatus::Succeeded);
    }

    #[test]
    fn finish_clears_active_mutation() {
        let mut registry = TaskRegistry::new();
        let id = registry.begin(TaskKind::Install, "install".to_string());
        registry.active_mutation = Some(id);
        registry.finish(id, Err("boom".to_string()));
        assert_eq!(registry.active_mutation, None);
        assert!(matches!(
            registry.get(id).unwrap().status,
            TaskStatus::Failed(_)
        ));
    }

    #[test]
    fn prune_keeps_running_and_recent() {
        let mut registry = TaskRegistry::new();
        let running = registry.begin(TaskKind::Install, "keep".to_string());
        for index in 0..(FINISHED_RETAIN + 10) {
            let id = registry.begin(TaskKind::Search, format!("s{index}"));
            registry.finish(id, Ok(()));
        }
        let _ = registry.begin(TaskKind::Search, "new".to_string());
        assert!(registry.get(running).is_some());
        let finished = registry.tasks().iter().filter(|t| !t.running()).count();
        assert!(finished <= FINISHED_RETAIN);
    }
}
