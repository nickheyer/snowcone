//! TUI application state and event loop.

use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use ratatui::widgets::{ListState, TableState};
use snowcone_core::{DatabaseGroup, HostInfo, Operation, PackageSummary, Registry};
use tokio::sync::mpsc;

use crate::commands::{ManagerStatus, manager_statuses};

use super::{event, ui};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Managers,
    Packages,
    Search,
}

/// Messages from background tasks back into the app loop.
pub enum TuiMsg {
    Results {
        query: String,
        packages: Vec<PackageSummary>,
    },
    Log(String),
}

pub async fn run(host: HostInfo, registry: Registry) -> anyhow::Result<()> {
    let registered_total = registry.factories().len();
    let (rows, groups) = manager_statuses(&registry, &host);
    // The sidebar only shows what's actually on this host; `snow managers`
    // lists the rest.
    let available: Vec<ManagerStatus> = rows.into_iter().filter(|row| row.available).collect();
    let mut terminal = ratatui::init();
    let result = App::new(host, available, registered_total, groups)
        .main_loop(&mut terminal)
        .await;
    ratatui::restore();
    result
}

pub struct App {
    pub host: HostInfo,
    pub manager_rows: Vec<ManagerStatus>,
    pub registered_total: usize,
    pub groups: Arc<Vec<DatabaseGroup>>,
    pub focus: Focus,
    pub search_input: String,
    pub searching: bool,
    pub packages: Vec<PackageSummary>,
    pub manager_list: ListState,
    pub package_table: TableState,
    pub log: Vec<String>,
    msg_tx: mpsc::UnboundedSender<TuiMsg>,
    msg_rx: mpsc::UnboundedReceiver<TuiMsg>,
    should_quit: bool,
}

impl App {
    fn new(
        host: HostInfo,
        manager_rows: Vec<ManagerStatus>,
        registered_total: usize,
        groups: Vec<DatabaseGroup>,
    ) -> Self {
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        let mut manager_list = ListState::default();
        if !manager_rows.is_empty() {
            manager_list.select(Some(0));
        }
        Self {
            host,
            manager_rows,
            registered_total,
            groups: Arc::new(groups),
            focus: Focus::Search,
            search_input: String::new(),
            searching: false,
            packages: Vec::new(),
            manager_list,
            package_table: TableState::default(),
            log: Vec::new(),
            msg_tx,
            msg_rx,
            should_quit: false,
        }
    }

    async fn main_loop(mut self, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
        let mut input = event::input_channel();
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        while !self.should_quit {
            terminal.draw(|frame| ui::draw(frame, &mut self))?;
            tokio::select! {
                maybe_event = input.recv() => match maybe_event {
                    Some(event) => self.handle_terminal_event(event),
                    None => break,
                },
                Some(msg) = self.msg_rx.recv() => self.handle_msg(msg),
                _ = tick.tick() => {}
            }
        }
        Ok(())
    }

    pub fn selected_package(&self) -> Option<&PackageSummary> {
        self.package_table
            .selected()
            .and_then(|index| self.packages.get(index))
    }

    fn handle_terminal_event(&mut self, event: Event) {
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press {
                self.handle_key(key);
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match self.focus {
            Focus::Search => match key.code {
                KeyCode::Esc => self.focus = Focus::Packages,
                KeyCode::Enter => self.start_search(),
                KeyCode::Backspace => {
                    self.search_input.pop();
                }
                KeyCode::Char(c) => self.search_input.push(c),
                _ => {}
            },
            _ => match key.code {
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Char('/') => self.focus = Focus::Search,
                KeyCode::Tab => self.cycle_focus(),
                KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                _ => {}
            },
        }
    }

    fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Managers => Focus::Packages,
            Focus::Packages => Focus::Search,
            Focus::Search => Focus::Managers,
        };
    }

    fn move_selection(&mut self, delta: i32) {
        match self.focus {
            Focus::Managers => {
                let next = bump(self.manager_list.selected(), self.manager_rows.len(), delta);
                self.manager_list.select(next);
            }
            Focus::Packages => {
                let next = bump(self.package_table.selected(), self.packages.len(), delta);
                self.package_table.select(next);
            }
            Focus::Search => {}
        }
    }

    /// Fan the query out to every search-capable backend in a background
    /// task; results come back through the message channel.
    fn start_search(&mut self) {
        let query = self.search_input.trim().to_string();
        if query.is_empty() || self.searching {
            return;
        }
        if self.groups.is_empty() {
            self.push_log("no backends available — nothing to search".to_string());
            return;
        }
        self.searching = true;
        let groups = Arc::clone(&self.groups);
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let mut all = Vec::new();
            for group in groups.iter() {
                // One query per database: its elected search-capable member.
                let Some(manager) = group.elect(Operation::Search) else {
                    continue;
                };
                match manager.search(&query).await {
                    Ok(packages) => all.extend(
                        packages
                            .iter()
                            .map(|package| PackageSummary::new(package.as_ref())),
                    ),
                    Err(error) => {
                        let _ = tx.send(TuiMsg::Log(format!(
                            "{}: search failed: {error}",
                            manager.id()
                        )));
                    }
                }
            }
            all.sort_by(|a, b| a.name.cmp(&b.name).then(a.manager.cmp(&b.manager)));
            let _ = tx.send(TuiMsg::Results {
                query,
                packages: all,
            });
        });
    }

    fn handle_msg(&mut self, msg: TuiMsg) {
        match msg {
            TuiMsg::Results { query, packages } => {
                self.searching = false;
                self.push_log(format!("{} result(s) for '{query}'", packages.len()));
                self.packages = packages;
                self.package_table.select(if self.packages.is_empty() {
                    None
                } else {
                    Some(0)
                });
                self.focus = Focus::Packages;
            }
            TuiMsg::Log(line) => self.push_log(line),
        }
    }

    fn push_log(&mut self, line: String) {
        self.log.push(line);
        if self.log.len() > 200 {
            self.log.remove(0);
        }
    }

    pub fn status_line(&self) -> String {
        if self.searching {
            return "searching…".to_string();
        }
        self.log.last().cloned().unwrap_or_default()
    }
}

fn bump(current: Option<usize>, len: usize, delta: i32) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let current = current.unwrap_or(0) as i32;
    Some((current + delta).rem_euclid(len as i32) as usize)
}
