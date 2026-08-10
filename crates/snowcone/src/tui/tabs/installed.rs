//! Installed tab: everything on the system, aggregated across enabled
//! managers, with a client-side filter.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::Span;
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, Wrap};

use crate::tui::app::{App, Tab};
use crate::tui::packages::{LoadState, SortKey};
use crate::tui::ui;

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let detail_height = ui::detail_height(app, area);
    let show_filter = ui::filter_bar_visible(app, Tab::Installed);
    let mut constraints = vec![Constraint::Min(0), Constraint::Length(detail_height)];
    if show_filter {
        constraints.insert(0, Constraint::Length(3));
    }
    let chunks = Layout::vertical(constraints).split(area);
    let (filter_area, table_area, detail_area) = if show_filter {
        (Some(chunks[0]), chunks[1], chunks[2])
    } else {
        (None, chunks[0], chunks[1])
    };

    if let Some(filter_area) = filter_area {
        ui::draw_filter_bar(frame, app, Tab::Installed, filter_area);
    }
    draw_table(frame, app, table_area);
    ui::draw_detail(frame, app, &app.tab, detail_area);
}

fn draw_table(frame: &mut Frame, app: &mut App, area: Rect) {
    let tab = &app.installed;
    let list = &tab.list;
    let title = format!(
        " Installed ({}{}) ",
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
        let text = match &tab.load {
            LoadState::NotLoaded | LoadState::Loading(_) => {
                format!("Loading installed packages… {}", ui::spinner(app.tick))
            }
            LoadState::Failed(error) => format!("Listing failed: {error}\n\nPress r to retry."),
            LoadState::Loaded if list.total_len() > 0 => "No rows match the filter.".to_string(),
            LoadState::Loaded => "Nothing installed via the enabled managers.".to_string(),
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
        Cell::from("SIZE"),
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
                Cell::from(
                    package
                        .installed_size
                        .map(ui::human_size)
                        .unwrap_or_else(|| "-".to_string()),
                ),
                Cell::from(
                    Span::from(package.description.clone().unwrap_or_default()).fg(Color::DarkGray),
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
            Constraint::Length(10),
            Constraint::Min(20),
        ],
    )
    .header(header)
    .block(block)
    .row_highlight_style(Style::new().reversed());
    frame.render_stateful_widget(table, area, &mut app.installed.list.table);
}
