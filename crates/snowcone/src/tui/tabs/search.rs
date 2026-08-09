//! Search tab: query bar, fan-out results, detail pane.

use std::collections::BTreeSet;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::Span;
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, Wrap};

use crate::tui::app::{App, InputTarget, Mode};
use crate::tui::packages::{PackageList, SortKey};
use crate::tui::tasks::TaskId;
use crate::tui::ui;

pub struct SearchTab {
    pub input: String,
    pub last_query: String,
    /// Manager ids the running query's `@manager` tokens named.
    pub restrict: BTreeSet<String>,
    pub epoch: u64,
    pub in_flight: Option<TaskId>,
    pub errors: Vec<String>,
    pub list: PackageList,
}

impl SearchTab {
    pub fn new() -> Self {
        let mut list = PackageList::new();
        // Best match first; `s` still cycles to the other sort keys.
        list.sort = SortKey::Relevance;
        Self {
            input: String::new(),
            last_query: String::new(),
            restrict: BTreeSet::new(),
            epoch: 0,
            in_flight: None,
            errors: Vec::new(),
            list,
        }
    }
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let detail_height = ui::detail_height(app, area);
    let [bar, table_area, detail] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(detail_height),
    ])
    .areas(area);

    draw_query_bar(frame, app, bar);
    draw_results(frame, app, table_area);
    ui::draw_detail(frame, app, &app.tab, detail);
}

fn draw_query_bar(frame: &mut Frame, app: &App, area: Rect) {
    let editing = matches!(app.mode, Mode::Input(InputTarget::SearchQuery));
    let mut title = if app.search.restrict.is_empty() {
        String::from(" Search (/) ")
    } else {
        let only: Vec<&str> = app
            .search
            .restrict
            .iter()
            .map(String::as_str)
            .collect();
        format!(" Search (/) - only {} ", only.join(", "))
    };
    if app.search.in_flight.is_some() {
        title = format!(" Search {} ", ui::spinner(app.tick));
    }
    let block = Block::bordered()
        .title(title)
        .border_style(ui::border_style(editing));
    let paragraph = Paragraph::new(app.search.input.as_str()).block(block);
    frame.render_widget(paragraph, area);
    if editing {
        frame.set_cursor_position((
            area.x + 1 + app.search.input.chars().count() as u16,
            area.y + 1,
        ));
    }
}

fn draw_results(frame: &mut Frame, app: &mut App, area: Rect) {
    let list = &app.search.list;
    let title = format!(
        " Packages ({}{}) ",
        list.visible_len(),
        if list.marked.is_empty() {
            String::new()
        } else {
            format!(", {} marked", list.marked.len())
        }
    );
    let block = Block::bordered()
        .title(title)
        .border_style(ui::border_style(false));

    if list.visible_len() == 0 {
        let text = if app.search.in_flight.is_some() {
            format!("Searching every enabled manager… {}", ui::spinner(app.tick))
        } else if list.total_len() > 0 {
            "No rows match the filter.".to_string()
        } else if app.search.last_query.is_empty() {
            "Type your query and press Enter - it fans out to every enabled package \
             manager at once. Prefix @manager to narrow (e.g. `@apt vim`)."
                .to_string()
        } else {
            format!("No results for '{}'.", app.search.last_query)
        };
        let paragraph = Paragraph::new(text)
            .style(Style::new().fg(Color::DarkGray))
            .wrap(Wrap { trim: true })
            .block(block);
        frame.render_widget(paragraph, area);
        return;
    }

    let header = Row::new([
        Cell::from(" "),
        Cell::from(format!("MANAGER{}", list.sort_indicator(SortKey::Manager))),
        Cell::from(format!("NAME{}", list.sort_indicator(SortKey::Name))),
        Cell::from(format!("VERSION{}", list.sort_indicator(SortKey::Version))),
        Cell::from(format!("STATE{}", list.sort_indicator(SortKey::State))),
        Cell::from("DESCRIPTION"),
    ])
    .style(Style::new().bold().fg(Color::Cyan));

    let rows: Vec<Row> = list
        .visible_rows()
        .map(|package| {
            Row::new(vec![
                Cell::from(if list.is_marked(package) {
                    Span::from("●").fg(Color::Magenta)
                } else {
                    Span::from(" ")
                }),
                Cell::from(package.manager.clone()),
                Cell::from(Span::from(package.name.clone()).bold()),
                Cell::from(package.version.clone().unwrap_or_else(|| "-".to_string())),
                Cell::from(ui::state_span(package.state)),
                Cell::from(
                    Span::from(package.description.clone().unwrap_or_default())
                        .fg(Color::DarkGray),
                ),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Length(10),
            Constraint::Length(30),
            Constraint::Length(16),
            Constraint::Length(12),
            Constraint::Min(20),
        ],
    )
    .header(header)
    .block(block)
    .row_highlight_style(Style::new().reversed());
    frame.render_stateful_widget(table, area, &mut app.search.list.table);
}
