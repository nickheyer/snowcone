//! TUI application state and event loop.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyEventKind};
use ratatui::DefaultTerminal;
use snowcone_core::{HostInfo, InstallState, Operation, PackageSummary, Registry};
use tokio::sync::mpsc;

use crate::commands::{ManagerStatus, manager_statuses};
use crate::config::Config;

use super::event::InputReader;
use super::fetch::{self, ListTarget};
use super::keys::{self, Action, KeyCtx, ModeKind};
use super::modal::{ConfirmState, Pending};
use super::packages::{ListTab, LoadState, PkgKey, key_of};
use super::pool::ManagerPool;
use super::policy::ExecMode;
use super::tabs::managers::ManagersTab;
use super::tabs::search::SearchTab;
use super::tabs::tasks::TasksTab;
use super::tasks::{LineSource, OutputLine, TaskId, TaskKind, TaskRegistry};
use super::{exec, trace, ui};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Search,
    Installed,
    Outdated,
    Managers,
    Tasks,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Search,
        Tab::Installed,
        Tab::Outdated,
        Tab::Managers,
        Tab::Tasks,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Search => "Search",
            Tab::Installed => "Installed",
            Tab::Outdated => "Outdated",
            Tab::Managers => "Managers",
            Tab::Tasks => "Tasks",
        }
    }

    pub fn index(self) -> usize {
        Tab::ALL.iter().position(|&tab| tab == self).unwrap()
    }

    pub fn next(self) -> Self {
        Tab::ALL[(self.index() + 1) % Tab::ALL.len()]
    }

    pub fn prev(self) -> Self {
        Tab::ALL[(self.index() + Tab::ALL.len() - 1) % Tab::ALL.len()]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputTarget {
    SearchQuery,
    Filter,
}

pub enum Mode {
    Normal,
    Input(InputTarget),
    Confirm(ConfirmState),
    Help { scroll: u16 },
}

impl Mode {
    pub fn kind(&self) -> ModeKind {
        match self {
            Mode::Normal => ModeKind::Normal,
            Mode::Input(_) => ModeKind::Input,
            Mode::Confirm(_) => ModeKind::Confirm,
            Mode::Help { .. } => ModeKind::Help,
        }
    }
}

pub enum InfoEntry {
    Loading,
    Loaded(Box<PackageSummary>),
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warn,
    Error,
}

/// Messages from background tasks back into the app loop.
pub enum TuiMsg {
    SearchBatch {
        epoch: u64,
        packages: Vec<PackageSummary>,
    },
    SearchDone {
        task: TaskId,
        epoch: u64,
        errors: Vec<String>,
    },
    Listed {
        task: TaskId,
        target: ListTarget,
        epoch: u64,
        packages: Vec<PackageSummary>,
        errors: Vec<String>,
    },
    Info {
        key: PkgKey,
        result: Result<Box<PackageSummary>, String>,
    },
    TaskOutput {
        id: TaskId,
        line: OutputLine,
    },
    TaskProgress {
        id: TaskId,
        current: u64,
        total: u64,
    },
    /// Mutations only; reads close through their own message.
    TaskDone {
        id: TaskId,
        result: Result<(), String>,
    },
    Log(String),
}

pub async fn run(host: HostInfo, registry: Registry) -> anyhow::Result<()> {
    let (msg_tx, msg_rx) = mpsc::unbounded_channel();
    // Tracing must land in the log pane, never on stderr under the
    // alternate screen.
    trace::init(msg_tx.clone());
    swallow_sigint()?;
    let (config, config_warning) = Config::load();
    let registered_total = registry.factories().len();
    let (statuses, groups) = manager_statuses(&registry, &host);
    let mut terminal = ratatui::init();
    let mut input = InputReader::spawn();
    let mut app = App::new(
        host,
        registry,
        config,
        statuses,
        registered_total,
        groups,
        msg_tx,
        msg_rx,
    );
    if let Some(warning) = config_warning {
        app.warn(warning);
    }
    let result = app.main_loop(&mut terminal, &mut input).await;
    ratatui::restore();
    result
}

/// In raw mode Ctrl-C arrives as a key event, so this handler is inert.
/// It exists for suspended mutations: Ctrl-C then goes to the foreground
/// process group - the child dies (as the user intended), snow survives
/// to report the failure and restore the TUI. A real handler (unlike
/// SIG_IGN) resets to default across exec, so children keep normal
/// Ctrl-C behavior.
fn swallow_sigint() -> anyhow::Result<()> {
    let mut sigint =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    tokio::spawn(async move {
        loop {
            sigint.recv().await;
        }
    });
    Ok(())
}

pub struct App {
    pub host: HostInfo,
    backend_registry: Registry,
    pub config: Config,
    pub pool: ManagerPool,
    pub statuses: Vec<ManagerStatus>,
    pub registered_total: usize,

    pub tab: Tab,
    pub mode: Mode,
    pub search: SearchTab,
    pub installed: ListTab,
    pub outdated: ListTab,
    pub managers: ManagersTab,
    pub tasks_view: TasksTab,

    pub tasks: TaskRegistry,
    pending_plans: VecDeque<super::pool::MutationPlan>,

    pub info_cache: HashMap<PkgKey, InfoEntry>,
    info_order: VecDeque<PkgKey>,
    detail_pending: Option<(PkgKey, Instant)>,
    pub detail_expanded: bool,

    pub log: Vec<String>,
    pub status: Option<(String, Severity)>,
    pub tick: u64,

    msg_tx: mpsc::UnboundedSender<TuiMsg>,
    msg_rx: mpsc::UnboundedReceiver<TuiMsg>,
    should_quit: bool,
}

const INFO_CACHE_CAP: usize = 512;
const LOG_CAP: usize = 500;

impl App {
    #[allow(clippy::too_many_arguments)]
    fn new(
        host: HostInfo,
        backend_registry: Registry,
        config: Config,
        statuses: Vec<ManagerStatus>,
        registered_total: usize,
        groups: Vec<snowcone_core::DatabaseGroup>,
        msg_tx: mpsc::UnboundedSender<TuiMsg>,
        msg_rx: mpsc::UnboundedReceiver<TuiMsg>,
    ) -> Self {
        Self {
            host,
            backend_registry,
            config,
            pool: ManagerPool::new(groups),
            statuses,
            registered_total,
            tab: Tab::Search,
            // Normal mode at launch: every keybind works immediately; `/`
            // enters the search input deliberately.
            mode: Mode::Normal,
            search: SearchTab::new(),
            installed: ListTab::new(),
            outdated: ListTab::new(),
            managers: ManagersTab::new(),
            tasks_view: TasksTab::new(),
            tasks: TaskRegistry::new(),
            pending_plans: VecDeque::new(),
            info_cache: HashMap::new(),
            info_order: VecDeque::new(),
            detail_pending: None,
            detail_expanded: false,
            log: Vec::new(),
            status: None,
            tick: 0,
            msg_tx,
            msg_rx,
            should_quit: false,
        }
    }

    async fn main_loop(
        mut self,
        terminal: &mut DefaultTerminal,
        input: &mut InputReader,
    ) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(Duration::from_millis(200));
        while !self.should_quit {
            terminal.draw(|frame| ui::draw(frame, &mut self))?;
            tokio::select! {
                maybe_event = input.recv() => match maybe_event {
                    Some(event) => self.handle_terminal_event(event, terminal, input).await,
                    None => break,
                },
                Some(msg) = self.msg_rx.recv() => {
                    self.handle_msg(msg);
                    // Drain-batch bursts (streamed task output) so a busy
                    // child costs one redraw per batch, not per line.
                    let mut budget = 256;
                    while budget > 0 {
                        match self.msg_rx.try_recv() {
                            Ok(msg) => {
                                self.handle_msg(msg);
                                budget -= 1;
                            }
                            Err(_) => break,
                        }
                    }
                    self.dispatch_pending(terminal, input).await;
                }
                _ = ticker.tick() => {
                    self.tick = self.tick.wrapping_add(1);
                    self.maybe_spawn_info();
                }
            }
        }
        Ok(())
    }

    async fn handle_terminal_event(
        &mut self,
        event: Event,
        terminal: &mut DefaultTerminal,
        input: &InputReader,
    ) {
        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
            && let Some(action) = keys::map_key(self.key_ctx(), key)
        {
            self.apply(action, terminal, input).await;
        }
    }

    fn key_ctx(&self) -> KeyCtx {
        KeyCtx {
            mode: self.mode.kind(),
            tab: self.tab,
            tasks_output_focused: self.tasks_view.output_focused,
        }
    }

    async fn apply(&mut self, action: Action, terminal: &mut DefaultTerminal, input: &InputReader) {
        match action {
            Action::CtrlC => {
                let in_quit_modal = matches!(
                    &self.mode,
                    Mode::Confirm(state) if matches!(state.pending, Pending::Quit)
                );
                if in_quit_modal {
                    self.tasks.abort_all();
                    self.should_quit = true;
                } else {
                    self.request_quit();
                }
            }
            Action::Quit => self.request_quit(),
            Action::Help => self.mode = Mode::Help { scroll: 0 },
            Action::GoTab(tab) => self.set_tab(tab),
            Action::NextTab => self.set_tab(self.tab.next()),
            Action::PrevTab => self.set_tab(self.tab.prev()),
            Action::Move(delta) => self.move_selection(delta),
            Action::Page(delta) => self.page_selection(delta),
            Action::Home => self.selection_home(),
            Action::End => self.selection_end(),
            Action::Reload => self.reload_current_tab(),
            Action::RefreshDbs => match self
                .pool
                .plan_all(Operation::Refresh, &self.config.disabled_set())
            {
                Ok(plans) => self.open_confirm(Pending::Mutations(plans)),
                Err(error) => self.warn(error),
            },
            Action::Escape => self.escape_tier(),
            Action::EnterInput => {
                self.mode = Mode::Input(match self.tab {
                    Tab::Search => InputTarget::SearchQuery,
                    _ => InputTarget::Filter,
                });
            }
            Action::ToggleMark => {
                if let Some(list) = self.current_list_mut() {
                    list.toggle_mark();
                }
            }
            Action::MarkAllVisible => {
                if let Some(list) = self.current_list_mut() {
                    list.mark_all_visible();
                    let marked = list.marked.len();
                    self.info(format!("{marked} marked"));
                }
            }
            Action::CycleSort => {
                if let Some(list) = self.current_list_mut() {
                    list.cycle_sort();
                    let label = list.sort.label();
                    let desc = list.sort_desc;
                    self.info(format!(
                        "sorted by {label} {}",
                        if desc { "▼" } else { "▲" }
                    ));
                }
            }
            Action::ToggleSortDir => {
                if let Some(list) = self.current_list_mut() {
                    list.toggle_sort_dir();
                }
            }
            Action::ToggleDetail => self.detail_expanded = !self.detail_expanded,
            Action::Install => self.request_mutation(Operation::Install),
            Action::Remove => self.request_mutation(Operation::Remove),
            Action::Upgrade => self.request_mutation(Operation::Upgrade),
            Action::UpgradeAll => match self
                .pool
                .plan_all(Operation::Upgrade, &self.config.disabled_set())
            {
                Ok(plans) => self.open_confirm(Pending::Mutations(plans)),
                Err(error) => self.warn(error),
            },
            Action::ToggleManager => self.toggle_selected_manager(),
            Action::TasksFocusOutput => {
                self.tasks_view.output_focused = true;
            }
            Action::TasksUnfocusOutput => {
                self.tasks_view.output_focused = false;
            }
            Action::TasksToggleFollow => {
                self.tasks_view.follow = !self.tasks_view.follow;
            }
            Action::TasksCancel => self.request_task_cancel(),
            Action::TasksClearFinished => {
                self.tasks.clear_finished();
                self.tasks_view.table.select(Some(0));
                self.info("finished tasks cleared");
            }
            Action::InputChar(c) => {
                let Mode::Input(target) = &self.mode else {
                    return;
                };
                match target {
                    InputTarget::SearchQuery => self.search.input.push(c),
                    InputTarget::Filter => {
                        if let Some(list) = self.current_list_mut() {
                            list.filter_push(c);
                        }
                    }
                }
            }
            Action::InputBackspace => {
                let Mode::Input(target) = &self.mode else {
                    return;
                };
                match target {
                    InputTarget::SearchQuery => {
                        self.search.input.pop();
                    }
                    InputTarget::Filter => {
                        if let Some(list) = self.current_list_mut() {
                            list.filter_pop();
                        }
                    }
                }
            }
            Action::InputClear => {
                let Mode::Input(target) = &self.mode else {
                    return;
                };
                match target {
                    InputTarget::SearchQuery => self.search.input.clear(),
                    InputTarget::Filter => {
                        if let Some(list) = self.current_list_mut() {
                            list.filter_clear();
                        }
                    }
                }
            }
            Action::InputSubmit => {
                let Mode::Input(target) = &self.mode else {
                    return;
                };
                let target = *target;
                self.mode = Mode::Normal;
                if target == InputTarget::SearchQuery {
                    let query = self.search.input.clone();
                    self.start_search(query);
                }
            }
            Action::InputCancel => self.mode = Mode::Normal,
            Action::ConfirmYes => self.confirm_resolve(true, terminal, input).await,
            Action::ConfirmNo => self.mode = Mode::Normal,
            Action::ConfirmMove => {
                if let Mode::Confirm(state) = &mut self.mode {
                    state.yes_selected = !state.yes_selected;
                }
            }
            Action::ConfirmActivate => {
                let yes = matches!(&self.mode, Mode::Confirm(state) if state.yes_selected);
                self.confirm_resolve(yes, terminal, input).await;
            }
            Action::HelpClose => self.mode = Mode::Normal,
            Action::HelpScroll(delta) => {
                if let Mode::Help { scroll } = &mut self.mode {
                    *scroll = (*scroll as i64 + delta).clamp(0, 500) as u16;
                }
            }
            Action::Hint(text) => self.info(text),
        }
    }

    // ---- navigation -----------------------------------------------------

    fn current_list_mut(&mut self) -> Option<&mut super::packages::PackageList> {
        match self.tab {
            Tab::Search => Some(&mut self.search.list),
            Tab::Installed => Some(&mut self.installed.list),
            Tab::Outdated => Some(&mut self.outdated.list),
            _ => None,
        }
    }

    fn current_list(&self) -> Option<&super::packages::PackageList> {
        match self.tab {
            Tab::Search => Some(&self.search.list),
            Tab::Installed => Some(&self.installed.list),
            Tab::Outdated => Some(&self.outdated.list),
            _ => None,
        }
    }

    fn move_selection(&mut self, delta: i64) {
        match self.tab {
            Tab::Search | Tab::Installed | Tab::Outdated => {
                if let Some(list) = self.current_list_mut() {
                    list.move_selection(delta);
                }
            }
            Tab::Managers => {
                let len = self.statuses.len();
                bump(&mut self.managers.table, len, delta);
            }
            Tab::Tasks => {
                if self.tasks_view.output_focused {
                    self.tasks_view.follow = false;
                    self.tasks_view.scroll =
                        (self.tasks_view.scroll as i64 + delta).max(0) as u16;
                } else {
                    let len = self.tasks.tasks().len() + 1;
                    bump(&mut self.tasks_view.table, len, delta);
                }
            }
        }
    }

    fn page_selection(&mut self, delta: i64) {
        match self.tab {
            Tab::Search | Tab::Installed | Tab::Outdated => {
                if let Some(list) = self.current_list_mut() {
                    list.page_selection(delta);
                }
            }
            Tab::Tasks if self.tasks_view.output_focused => {
                self.tasks_view.follow = false;
                self.tasks_view.scroll =
                    (self.tasks_view.scroll as i64 + delta).max(0) as u16;
            }
            _ => self.move_selection(delta.signum()),
        }
    }

    fn selection_home(&mut self) {
        match self.tab {
            Tab::Search | Tab::Installed | Tab::Outdated => {
                if let Some(list) = self.current_list_mut() {
                    list.select_home();
                }
            }
            Tab::Managers => self.managers.table.select(Some(0)),
            Tab::Tasks => {
                if self.tasks_view.output_focused {
                    self.tasks_view.follow = false;
                    self.tasks_view.scroll = 0;
                } else {
                    self.tasks_view.table.select(Some(0));
                }
            }
        }
    }

    fn selection_end(&mut self) {
        match self.tab {
            Tab::Search | Tab::Installed | Tab::Outdated => {
                if let Some(list) = self.current_list_mut() {
                    list.select_end();
                }
            }
            Tab::Managers => {
                let len = self.statuses.len();
                if len > 0 {
                    self.managers.table.select(Some(len - 1));
                }
            }
            Tab::Tasks => {
                if self.tasks_view.output_focused {
                    self.tasks_view.follow = true;
                } else {
                    self.tasks_view.table.select(Some(self.tasks.tasks().len()));
                }
            }
        }
    }

    fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        match tab {
            Tab::Installed => self.ensure_loaded(ListTarget::Installed),
            Tab::Outdated => self.ensure_loaded(ListTarget::Outdated),
            _ => {}
        }
    }

    /// Esc, in priority order: cancel this tab's in-flight fetch, clear
    /// marks, clear the filter.
    fn escape_tier(&mut self) {
        match self.tab {
            Tab::Search => {
                if let Some(task) = self.search.in_flight.take() {
                    self.tasks.cancel(task);
                    self.info("search cancelled");
                    return;
                }
                if self.search.list.clear_marks() {
                    self.info("marks cleared");
                }
            }
            Tab::Installed | Tab::Outdated => {
                let target = if self.tab == Tab::Installed {
                    ListTarget::Installed
                } else {
                    ListTarget::Outdated
                };
                let loading = {
                    let tab = self.list_tab_mut(target);
                    if let LoadState::Loading(task) = &tab.load {
                        let task = *task;
                        tab.load = LoadState::NotLoaded;
                        Some(task)
                    } else {
                        None
                    }
                };
                if let Some(task) = loading {
                    self.tasks.cancel(task);
                    self.info("loading cancelled");
                    return;
                }
                let cleared = {
                    let tab = self.list_tab_mut(target);
                    if tab.list.clear_marks() {
                        Some("marks cleared")
                    } else if tab.list.filter_clear() {
                        Some("filter cleared")
                    } else {
                        None
                    }
                };
                if let Some(message) = cleared {
                    self.info(message);
                }
            }
            _ => {}
        }
    }

    // ---- reads ----------------------------------------------------------

    fn list_tab_mut(&mut self, target: ListTarget) -> &mut ListTab {
        match target {
            ListTarget::Installed => &mut self.installed,
            ListTarget::Outdated => &mut self.outdated,
        }
    }

    fn ensure_loaded(&mut self, target: ListTarget) {
        if matches!(self.list_tab_mut(target).load, LoadState::NotLoaded) {
            self.spawn_list_load(target);
        }
    }

    fn spawn_list_load(&mut self, target: ListTarget) {
        let prior = {
            let tab = self.list_tab_mut(target);
            tab.epoch += 1;
            if let LoadState::Loading(task) = &tab.load {
                Some(*task)
            } else {
                None
            }
        };
        if let Some(task) = prior {
            self.tasks.cancel(task);
        }
        let epoch = self.list_tab_mut(target).epoch;
        let task = self
            .tasks
            .begin(TaskKind::List, format!("list {} packages", target.label()));
        let handle = fetch::spawn_list(
            Arc::clone(&self.pool.groups),
            self.config.disabled_set(),
            target,
            epoch,
            task,
            self.msg_tx.clone(),
        );
        self.tasks.set_abort(task, handle.abort_handle());
        self.list_tab_mut(target).load = LoadState::Loading(task);
    }

    fn start_search(&mut self, query: String) {
        let query = query.trim().to_string();
        if query.is_empty() {
            self.info("type a query first (/)");
            return;
        }
        if let Some(task) = self.search.in_flight.take() {
            self.tasks.cancel(task);
        }
        self.search.epoch += 1;
        self.search.errors.clear();
        self.search.list.set_rows(Vec::new());
        self.search.last_query = query.clone();
        let epoch = self.search.epoch;
        let task = self
            .tasks
            .begin(TaskKind::Search, format!("search '{query}'"));
        let handle = fetch::spawn_search(
            Arc::clone(&self.pool.groups),
            self.config.disabled_set(),
            query,
            epoch,
            task,
            self.msg_tx.clone(),
        );
        self.tasks.set_abort(task, handle.abort_handle());
        self.search.in_flight = Some(task);
    }

    fn reload_current_tab(&mut self) {
        match self.tab {
            Tab::Search => {
                if self.search.last_query.is_empty() {
                    self.info("nothing to re-run - search first (/)");
                } else {
                    let query = self.search.last_query.clone();
                    self.start_search(query);
                }
            }
            Tab::Installed => self.spawn_list_load(ListTarget::Installed),
            Tab::Outdated => self.spawn_list_load(ListTarget::Outdated),
            Tab::Managers => {
                let (statuses, groups) = manager_statuses(&self.backend_registry, &self.host);
                let available = statuses.iter().filter(|status| status.available).count();
                self.statuses = statuses;
                self.pool.swap(groups);
                self.invalidate_lists();
                let len = self.statuses.len();
                if let Some(selected) = self.managers.table.selected()
                    && selected >= len
                {
                    self.managers.table.select(len.checked_sub(1));
                }
                self.info(format!("re-probed: {available} manager(s) detected"));
            }
            Tab::Tasks => self.info("tasks update live"),
        }
    }

    fn maybe_spawn_info(&mut self) {
        let key = {
            let Some(list) = self.current_list() else {
                self.detail_pending = None;
                return;
            };
            let Some(selected) = list.selected() else {
                self.detail_pending = None;
                return;
            };
            key_of(selected)
        };
        if self.info_cache.contains_key(&key) {
            self.detail_pending = None;
            return;
        }
        match &self.detail_pending {
            Some((pending, since)) if *pending == key => {
                if since.elapsed() >= Duration::from_millis(300) {
                    self.cache_info(key.clone(), InfoEntry::Loading);
                    fetch::spawn_info(
                        Arc::clone(&self.pool.groups),
                        self.config.disabled_set(),
                        key,
                        self.msg_tx.clone(),
                    );
                    self.detail_pending = None;
                }
            }
            _ => self.detail_pending = Some((key, Instant::now())),
        }
    }

    fn cache_info(&mut self, key: PkgKey, entry: InfoEntry) {
        if self.info_cache.insert(key.clone(), entry).is_none() {
            self.info_order.push_back(key);
        }
        while self.info_order.len() > INFO_CACHE_CAP {
            if let Some(old) = self.info_order.pop_front() {
                self.info_cache.remove(&old);
            }
        }
    }

    // ---- mutations --------------------------------------------------------

    fn request_mutation(&mut self, operation: Operation) {
        let planned = {
            let Some(list) = self.current_list() else {
                return;
            };
            let targets = list.targets();
            let guard = targets.iter().find_map(|target| {
                let reason = match operation {
                    Operation::Install => match target.state {
                        InstallState::Installed | InstallState::Upgradable => {
                            Some("already installed")
                        }
                        _ => None,
                    },
                    Operation::Remove => match target.state {
                        InstallState::Available | InstallState::Unknown => Some("not installed"),
                        _ => None,
                    },
                    Operation::Upgrade => match target.state {
                        InstallState::Available => Some("not installed"),
                        _ => None,
                    },
                    _ => None,
                };
                reason.map(|reason| format!("{}: {reason}", target.name))
            });
            match guard {
                Some(message) => Err(message),
                None => self.pool.plan_mutation(
                    operation,
                    &targets,
                    &self.config.disabled_set(),
                ),
            }
        };
        match planned {
            Ok(plan) => self.open_confirm(Pending::Mutations(vec![plan])),
            Err(error) => self.warn(error),
        }
    }

    fn open_confirm(&mut self, pending: Pending) {
        if matches!(pending, Pending::Mutations(_)) && self.tasks.active_mutation.is_some() {
            self.warn("a mutation is already running - wait for it (Tasks tab)");
            return;
        }
        self.mode = Mode::Confirm(ConfirmState::new(pending));
    }

    async fn confirm_resolve(
        &mut self,
        yes: bool,
        terminal: &mut DefaultTerminal,
        input: &InputReader,
    ) {
        let Mode::Confirm(state) = std::mem::replace(&mut self.mode, Mode::Normal) else {
            return;
        };
        if !yes {
            return;
        }
        match state.pending {
            Pending::Quit => {
                self.tasks.abort_all();
                self.should_quit = true;
            }
            Pending::CancelTask(id, title) => {
                if self.tasks.cancel(id) {
                    self.note_cancelled_read(id);
                    self.info(format!("cancelled: {title}"));
                } else {
                    self.warn("task already finished");
                }
            }
            Pending::Mutations(plans) => {
                if self.tasks.active_mutation.is_some() {
                    self.warn("a mutation is already running - wait for it (Tasks tab)");
                    return;
                }
                self.pending_plans.extend(plans);
                self.dispatch_pending(terminal, input).await;
            }
        }
    }

    /// Start queued plans FIFO: captured ones spawn (one at a time -
    /// `TaskDone` re-enters here for the next), interactive ones run
    /// inline with the TUI suspended.
    async fn dispatch_pending(&mut self, terminal: &mut DefaultTerminal, input: &InputReader) {
        while self.tasks.active_mutation.is_none() {
            let Some(plan) = self.pending_plans.pop_front() else {
                break;
            };
            let id = self.begin_mutation_task(&plan);
            self.tasks.active_mutation = Some(id);
            match plan.mode {
                ExecMode::Captured => {
                    let handle = exec::spawn_captured(
                        plan,
                        Arc::clone(&self.pool.groups),
                        id,
                        self.msg_tx.clone(),
                    );
                    self.tasks.set_abort(id, handle.abort_handle());
                    break;
                }
                ExecMode::Interactive => {
                    let groups = Arc::clone(&self.pool.groups);
                    let result = exec::run_suspended(terminal, input, &groups, &plan).await;
                    self.tasks.push_output(
                        id,
                        OutputLine {
                            source: LineSource::Status,
                            text: "ran interactively on the terminal".to_string(),
                            transient: false,
                        },
                    );
                    self.finish_task(id, result);
                }
            }
        }
    }

    fn begin_mutation_task(&mut self, plan: &super::pool::MutationPlan) -> TaskId {
        let kind = TaskKind::from_operation(plan.operation);
        let id = self.tasks.begin(kind, plan.title.clone());
        if let Some(task) = self.tasks.get_mut(id) {
            task.manager = Some(plan.manager_id.clone());
            task.database = Some(plan.database);
            task.mode = Some(plan.mode);
            task.names = plan
                .requests
                .iter()
                .map(|request| request.name.clone())
                .collect();
        }
        id
    }

    fn finish_task(&mut self, id: TaskId, result: Result<(), String>) {
        let Some(task) = self.tasks.get(id) else {
            return;
        };
        let kind = task.kind;
        let title = task.title.clone();
        let database = task.database;
        let names = task.names.clone();
        let ok = result.is_ok();
        let error = result.as_ref().err().cloned();
        self.tasks.finish(id, result);
        match error {
            None => {
                self.info(format!("done: {title}"));
                self.push_log(format!("{title}: ok"));
            }
            Some(error) => {
                self.error(format!("{title}: {error}"));
                self.push_log(format!("{title}: FAILED - {error}"));
            }
        }
        if kind.is_mutation() {
            self.invalidate_lists();
            match self.tab {
                Tab::Installed => self.spawn_list_load(ListTarget::Installed),
                Tab::Outdated => self.spawn_list_load(ListTarget::Outdated),
                _ => {}
            }
            if ok && let Some(database) = database {
                self.flip_search_states(kind, database, &names);
            }
        }
    }

    /// A successful mutation makes the search tab's copies of those rows
    /// stale; flip their state optimistically instead of forcing a re-run.
    fn flip_search_states(&mut self, kind: TaskKind, database: &str, names: &[String]) {
        if names.is_empty() {
            // Upgrade-everything: no per-row certainty.
            return;
        }
        let new_state = match kind {
            TaskKind::Install | TaskKind::Upgrade => InstallState::Installed,
            TaskKind::Remove => InstallState::Available,
            _ => return,
        };
        let members: Vec<String> = self
            .pool
            .groups
            .iter()
            .find(|group| group.database == database)
            .map(|group| {
                group
                    .managers
                    .iter()
                    .map(|manager| manager.id().to_string())
                    .collect()
            })
            .unwrap_or_default();
        self.search.list.update_rows(|row| {
            if members.contains(&row.manager) && names.contains(&row.name) {
                row.state = new_state;
            }
        });
    }

    fn invalidate_lists(&mut self) {
        for target in [ListTarget::Installed, ListTarget::Outdated] {
            let loading = {
                let tab = self.list_tab_mut(target);
                let loading = if let LoadState::Loading(task) = &tab.load {
                    Some(*task)
                } else {
                    None
                };
                tab.load = LoadState::NotLoaded;
                loading
            };
            if let Some(task) = loading {
                self.tasks.cancel(task);
            }
        }
    }

    // ---- managers / tasks -------------------------------------------------

    fn toggle_selected_manager(&mut self) {
        let Some(status) = self
            .managers
            .table
            .selected()
            .and_then(|index| self.statuses.get(index))
        else {
            return;
        };
        let id = status.id.clone();
        let now_disabled = !self.config.is_disabled(&id);
        self.config.set_disabled(&id, now_disabled);
        if let Err(error) = self.config.save() {
            self.warn(format!("config save failed: {error}"));
        }
        self.invalidate_lists();
        if now_disabled {
            self.info(format!("{id} disabled - searches and listings skip it now"));
        } else {
            self.info(format!("{id} enabled"));
        }
    }

    fn request_task_cancel(&mut self) {
        let Some(selected) = self.tasks_view.table.selected() else {
            return;
        };
        if selected == 0 {
            self.info("that's the log - nothing to cancel");
            return;
        }
        let Some(task) = self.tasks.tasks().get(selected - 1) else {
            return;
        };
        if !task.running() {
            self.info("task already finished");
            return;
        }
        let id = task.id;
        let title = task.title.clone();
        if task.mode == Some(ExecMode::Interactive) {
            self.warn("interactive tasks own the terminal - Ctrl-C them there");
            return;
        }
        if task.kind.is_mutation() {
            // Killing a package manager mid-transaction deserves a modal.
            self.open_confirm(Pending::CancelTask(id, title));
            return;
        }
        if self.tasks.cancel(id) {
            self.note_cancelled_read(id);
            self.info(format!("cancelled: {title}"));
        }
    }

    /// Keep per-tab read state honest after a cancellation from the
    /// Tasks tab.
    fn note_cancelled_read(&mut self, id: TaskId) {
        if self.search.in_flight == Some(id) {
            self.search.in_flight = None;
        }
        for target in [ListTarget::Installed, ListTarget::Outdated] {
            let tab = self.list_tab_mut(target);
            if matches!(&tab.load, LoadState::Loading(task) if *task == id) {
                tab.load = LoadState::NotLoaded;
            }
        }
    }

    fn request_quit(&mut self) {
        if self.tasks.active_mutation.is_some() {
            self.open_confirm(Pending::Quit);
        } else {
            self.tasks.abort_all();
            self.should_quit = true;
        }
    }

    // ---- messages ---------------------------------------------------------

    fn handle_msg(&mut self, msg: TuiMsg) {
        match msg {
            TuiMsg::SearchBatch { epoch, packages } => {
                if epoch == self.search.epoch {
                    self.search.list.extend_rows(packages);
                }
            }
            TuiMsg::SearchDone {
                task,
                epoch,
                errors,
            } => {
                let result = if errors.is_empty() {
                    Ok(())
                } else {
                    Err(errors.join("; "))
                };
                self.tasks.finish(task, result);
                if epoch != self.search.epoch {
                    return;
                }
                if self.search.in_flight == Some(task) {
                    self.search.in_flight = None;
                }
                let mut status = format!(
                    "{} result(s) for '{}'",
                    self.search.list.total_len(),
                    self.search.last_query
                );
                if errors.is_empty() {
                    self.info(status);
                } else {
                    status.push_str(&format!(
                        " · {} manager(s) failed (Tasks tab)",
                        errors.len()
                    ));
                    self.warn(status);
                }
                self.search.errors = errors;
            }
            TuiMsg::Listed {
                task,
                target,
                epoch,
                packages,
                errors,
            } => {
                let total_failure = packages.is_empty() && !errors.is_empty();
                self.tasks.finish(
                    task,
                    if total_failure {
                        Err(errors.join("; "))
                    } else {
                        Ok(())
                    },
                );
                for error in &errors {
                    self.push_log(format!("list {}: {error}", target.label()));
                }
                let partial = !errors.is_empty() && !total_failure;
                {
                    let tab = self.list_tab_mut(target);
                    if epoch != tab.epoch {
                        return;
                    }
                    if total_failure {
                        tab.load = LoadState::Failed(errors.join("; "));
                    } else {
                        tab.list.set_rows(packages);
                        tab.load = LoadState::Loaded;
                    }
                }
                if partial {
                    self.warn(format!(
                        "{} listing: {} manager(s) failed (Tasks tab)",
                        target.label(),
                        errors.len()
                    ));
                }
            }
            TuiMsg::Info { key, result } => {
                let entry = match result {
                    Ok(summary) => InfoEntry::Loaded(summary),
                    Err(error) => InfoEntry::Failed(error),
                };
                self.cache_info(key, entry);
            }
            TuiMsg::TaskOutput { id, line } => self.tasks.push_output(id, line),
            TuiMsg::TaskProgress { id, current, total } => {
                self.tasks.set_progress(id, current, total);
            }
            TuiMsg::TaskDone { id, result } => self.finish_task(id, result),
            TuiMsg::Log(line) => self.push_log(line),
        }
    }

    // ---- status / log -------------------------------------------------------

    fn push_log(&mut self, line: String) {
        self.log.push(line);
        if self.log.len() > LOG_CAP {
            self.log.remove(0);
        }
    }

    fn info(&mut self, text: impl Into<String>) {
        self.status = Some((text.into(), Severity::Info));
    }

    fn warn(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.push_log(text.clone());
        self.status = Some((text, Severity::Warn));
    }

    fn error(&mut self, text: impl Into<String>) {
        self.status = Some((text.into(), Severity::Error));
    }
}

fn bump(table: &mut ratatui::widgets::TableState, len: usize, delta: i64) {
    if len == 0 {
        table.select(None);
        return;
    }
    let current = table.selected().unwrap_or(0) as i64;
    table.select(Some((current + delta).rem_euclid(len as i64) as usize));
}
