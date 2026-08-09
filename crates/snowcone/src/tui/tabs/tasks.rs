//! Tasks tab: every operation the TUI has run, with live streamed output.
//! Row 0 is the pinned "snowcone log" pseudo-task (app events + tracing).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, TableState};

use crate::tui::app::App;
use crate::tui::tasks::{LineSource, Task, TaskStatus};
use crate::tui::ui;

pub struct TasksTab {
    pub table: TableState,
    pub output_focused: bool,
    pub follow: bool,
    pub scroll: u16,
}

impl TasksTab {
    pub fn new() -> Self {
        let mut table = TableState::default();
        table.select(Some(0));
        Self {
            table,
            output_focused: false,
            follow: true,
            scroll: 0,
        }
    }
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let list_height = (area.height / 3).clamp(6, 12);
    let [list_area, output_area] =
        Layout::vertical([Constraint::Length(list_height), Constraint::Min(0)]).areas(area);
    draw_list(frame, app, list_area);
    draw_output(frame, app, output_area);
}

fn status_span(app: &App, task: &Task) -> Span<'static> {
    match &task.status {
        TaskStatus::Running => Span::from(ui::spinner(app.tick).to_string()).fg(Color::Cyan),
        TaskStatus::Succeeded => Span::from("✓").fg(Color::Green),
        TaskStatus::Failed(_) => Span::from("✗").fg(Color::Red),
        TaskStatus::Cancelled => Span::from("⊘").fg(Color::DarkGray),
    }
}

fn draw_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let running = app.tasks.running_count();
    let block = Block::bordered()
        .title(format!(" Tasks ({running} running) "))
        .border_style(ui::border_style(!app.tasks_view.output_focused));

    let header = Row::new(["", "TASK", "MANAGER", "ELAPSED"])
        .style(Style::new().bold().fg(Color::Cyan));
    let mut rows = vec![Row::new(vec![
        Cell::from(Span::from("≡").fg(Color::Cyan)),
        Cell::from("snowcone log"),
        Cell::from("-"),
        Cell::from("-"),
    ])];
    rows.extend(app.tasks.tasks().iter().map(|task| {
        Row::new(vec![
            Cell::from(status_span(app, task)),
            Cell::from(task.title.clone()),
            Cell::from(task.manager.clone().unwrap_or_else(|| "-".to_string())),
            Cell::from(ui::human_duration(task.elapsed())),
        ])
    }));

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Min(30),
            Constraint::Length(10),
            Constraint::Length(9),
        ],
    )
    .header(header)
    .block(block)
    .row_highlight_style(Style::new().reversed());
    frame.render_stateful_widget(table, area, &mut app.tasks_view.table);
}

fn draw_output(frame: &mut Frame, app: &App, area: Rect) {
    let selected = app.tasks_view.table.selected().unwrap_or(0);
    let (title, lines): (String, Vec<Line<'static>>) = if selected == 0 {
        (
            " snowcone log ".to_string(),
            app.log
                .iter()
                .map(|entry| Line::from(entry.clone()))
                .collect(),
        )
    } else {
        match app.tasks.tasks().get(selected - 1) {
            Some(task) => {
                let status = match &task.status {
                    TaskStatus::Running => "running".to_string(),
                    TaskStatus::Succeeded => "ok".to_string(),
                    TaskStatus::Failed(error) => format!("failed: {error}"),
                    TaskStatus::Cancelled => "cancelled".to_string(),
                };
                let mut lines: Vec<Line<'static>> = task
                    .output
                    .iter()
                    .map(|line| {
                        let style = match line.source {
                            LineSource::Stdout => Style::new(),
                            LineSource::Stderr => Style::new().fg(Color::Yellow),
                            LineSource::Status => Style::new().fg(Color::Cyan).italic(),
                        };
                        Line::from(Span::styled(line.text.clone(), style))
                    })
                    .collect();
                if let Some(mode) = task.mode
                    && lines.is_empty()
                {
                    lines.push(
                        Line::from(format!("(runs {})", mode.describe())).fg(Color::DarkGray),
                    );
                }
                if let TaskStatus::Failed(error) = &task.status {
                    lines.push(Line::from(Span::from(error.clone()).fg(Color::Red)));
                }
                (format!(" {} - {} ", task.title, status), lines)
            }
            None => (" output ".to_string(), Vec::new()),
        }
    };

    let inner_height = area.height.saturating_sub(2);
    let total = lines.len() as u16;
    let max_scroll = total.saturating_sub(inner_height);
    let scroll = if app.tasks_view.follow {
        max_scroll
    } else {
        app.tasks_view.scroll.min(max_scroll)
    };

    let follow_hint = if app.tasks_view.follow {
        " (following - f to stop) "
    } else {
        " (f to follow) "
    };
    let block = Block::bordered()
        .title(format!("{title}·{follow_hint}"))
        .border_style(ui::border_style(app.tasks_view.output_focused));
    let paragraph = Paragraph::new(lines).block(block).scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}
