//! Managers tab: every registered backend - detected or not - with
//! persisted enable/disable toggles and the effective election primary.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, TableState, Wrap};

use crate::tui::app::App;
use crate::tui::ui;

pub struct ManagersTab {
    pub table: TableState,
}

impl ManagersTab {
    pub fn new() -> Self {
        let mut table = TableState::default();
        table.select(Some(0));
        Self { table }
    }
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let [table_area, detail_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(5)]).areas(area);
    draw_table(frame, app, table_area);
    draw_detail(frame, app, detail_area);
}

fn draw_table(frame: &mut Frame, app: &mut App, area: Rect) {
    let available = app.statuses.iter().filter(|row| row.available).count();
    let block = Block::bordered()
        .title(format!(
            " Managers ({available} detected / {} registered) - Space toggles ",
            app.registered_total
        ))
        .border_style(ui::border_style(false));

    let disabled = app.config.disabled_set();
    let header = Row::new([
        "EN",
        "ID",
        "KIND",
        "DATABASE",
        "PRI",
        "CAPABILITIES",
        "DETAIL",
    ])
    .style(Style::new().bold().fg(Color::Cyan));
    let rows: Vec<Row> = app
        .statuses
        .iter()
        .map(|status| {
            let is_disabled = disabled.contains(&status.id);
            let enabled_cell = if !status.available {
                Cell::from(Span::from("·").fg(Color::DarkGray))
            } else if is_disabled {
                Cell::from(Span::from("○").fg(Color::DarkGray))
            } else {
                Cell::from(Span::from("●").fg(Color::Green))
            };
            let primary = status
                .database
                .as_deref()
                .and_then(|database| app.pool.effective_primary(database, &disabled))
                .is_some_and(|primary| primary == status.id);
            let dim = !status.available || is_disabled;
            let base = if dim {
                Style::new().fg(Color::DarkGray)
            } else {
                Style::new()
            };
            Row::new(vec![
                enabled_cell,
                Cell::from(Span::from(status.id.clone()).bold()),
                Cell::from(status.kind.clone().unwrap_or_default()),
                Cell::from(status.database.clone().unwrap_or_default()),
                Cell::from(if primary {
                    Span::from("★").fg(Color::Yellow)
                } else {
                    Span::from(" ")
                }),
                Cell::from(status.capabilities.join(" ")),
                Cell::from(status.detail.clone()),
            ])
            .style(base)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Length(14),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(4),
            Constraint::Min(24),
            Constraint::Min(20),
        ],
    )
    .header(header)
    .block(block)
    .row_highlight_style(Style::new().reversed());
    frame.render_stateful_widget(table, area, &mut app.managers.table);
}

fn draw_detail(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered()
        .title(" Details ")
        .border_style(ui::border_style(false));
    let Some(status) = app
        .managers
        .table
        .selected()
        .and_then(|index| app.statuses.get(index))
    else {
        frame.render_widget(
            Paragraph::new("Select a manager.")
                .style(Style::new().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return;
    };
    let disabled = app.config.is_disabled(&status.id);
    let mut lines = vec![Line::from(vec![
        Span::from(status.id.clone()).bold(),
        Span::from(if status.available {
            if disabled {
                "  disabled by you (Space re-enables)"
            } else {
                "  enabled"
            }
        } else {
            "  not detected"
        })
        .fg(if status.available && !disabled {
            Color::Green
        } else {
            Color::Yellow
        }),
    ])];
    lines.push(Line::from(vec![
        Span::from("where: ").fg(Color::DarkGray),
        Span::from(status.detail.clone()),
    ]));
    if let (Some(kind), Some(database)) = (&status.kind, &status.database) {
        lines.push(Line::from(vec![
            Span::from("kind: ").fg(Color::DarkGray),
            Span::from(kind.clone()),
            Span::from("   database: ").fg(Color::DarkGray),
            Span::from(database.clone()),
        ]));
    }
    if !status.capabilities.is_empty() {
        lines.push(Line::from(vec![
            Span::from("can: ").fg(Color::DarkGray),
            Span::from(status.capabilities.join(", ")),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(block),
        area,
    );
}
