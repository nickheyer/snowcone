//! TUI rendering.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, Paragraph, Row, Table, Wrap};
use snowcone_core::PackageSummary;

use super::app::{App, Focus};

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [header, search, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);
    draw_search(frame, app, search);

    let [sidebar, main] =
        Layout::horizontal([Constraint::Length(34), Constraint::Min(0)]).areas(body);
    draw_managers(frame, app, sidebar);

    let [table, detail] = Layout::vertical([Constraint::Min(0), Constraint::Length(9)]).areas(main);
    draw_packages(frame, app, table);
    draw_detail(frame, app, detail);

    draw_footer(frame, app, footer);
}

fn border_style(focused: bool) -> Style {
    if focused {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().fg(Color::DarkGray)
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let os = app
        .host
        .os
        .pretty_name
        .as_deref()
        .unwrap_or("unknown distro");
    let line = Line::from(vec![
        Span::from(" snowcone ").bold().fg(Color::Cyan),
        Span::from(format!("— {os} ({})", app.host.arch)).fg(Color::DarkGray),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_search(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Search;
    let paragraph = Paragraph::new(app.search_input.as_str()).block(
        Block::bordered()
            .title("Search (/)")
            .border_style(border_style(focused)),
    );
    frame.render_widget(paragraph, area);
    if focused {
        frame.set_cursor_position((
            area.x + 1 + app.search_input.chars().count() as u16,
            area.y + 1,
        ));
    }
}

fn draw_managers(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Managers;
    let block = Block::bordered()
        .title(format!("Managers ({})", app.manager_rows.len()))
        .border_style(border_style(focused));

    if app.manager_rows.is_empty() {
        let paragraph = Paragraph::new(
            "No backends registered yet.\n\nDiscovery, the interfaces, and this UI are wired; \
             backend crates are the next milestone.",
        )
        .style(Style::new().fg(Color::DarkGray))
        .wrap(Wrap { trim: true })
        .block(block);
        frame.render_widget(paragraph, area);
        return;
    }

    let items: Vec<ListItem> = app
        .manager_rows
        .iter()
        .map(|row| {
            let (dot, dot_style) = if row.available {
                ("● ", Style::new().fg(Color::Green))
            } else {
                ("○ ", Style::new().fg(Color::DarkGray))
            };
            let mut spans = vec![Span::styled(dot, dot_style), Span::from(row.id.clone()).bold()];
            if let Some(kind) = &row.kind {
                spans.push(Span::from(format!("  {kind}")).fg(Color::DarkGray));
            }
            let mut item = ListItem::new(Line::from(spans));
            if !row.available {
                item = item.style(Style::new().add_modifier(Modifier::DIM));
            }
            item
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, area, &mut app.manager_list);
}

fn draw_packages(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Packages;
    let block = Block::bordered()
        .title(format!("Packages ({})", app.packages.len()))
        .border_style(border_style(focused));

    if app.packages.is_empty() {
        let paragraph = Paragraph::new(
            "No results yet.\n\nPress / to search across every detected package manager at once.",
        )
        .style(Style::new().fg(Color::DarkGray))
        .wrap(Wrap { trim: true })
        .block(block);
        frame.render_widget(paragraph, area);
        return;
    }

    let header = Row::new(["MANAGER", "NAME", "VERSION", "DESCRIPTION"])
        .style(Style::new().bold().fg(Color::Cyan));
    let rows = app.packages.iter().map(|package| {
        Row::new(vec![
            package.manager.clone(),
            package.name.clone(),
            package.version.clone().unwrap_or_else(|| "-".to_string()),
            package.description.clone().unwrap_or_default(),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Length(28),
            Constraint::Length(14),
            Constraint::Min(20),
        ],
    )
    .header(header)
    .block(block)
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(table, area, &mut app.package_table);
}

fn draw_detail(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered()
        .title("Details")
        .border_style(border_style(false));
    let paragraph = match app.selected_package() {
        Some(package) => Paragraph::new(detail_lines(package)),
        None => Paragraph::new("Select a package to see its metadata.")
            .style(Style::new().fg(Color::DarkGray)),
    };
    frame.render_widget(paragraph.wrap(Wrap { trim: true }).block(block), area);
}

fn detail_lines(package: &PackageSummary) -> Vec<Line<'_>> {
    let mut lines = vec![Line::from(vec![
        Span::from(package.name.as_str()).bold(),
        Span::from(" "),
        Span::from(package.version.as_deref().unwrap_or("-")),
        Span::from(format!("  [{}]", package.manager)).fg(Color::DarkGray),
        Span::from(format!("  {}", package.state)).fg(Color::Yellow),
    ])];
    let mut field = |label: &str, value: Option<&str>| {
        if let Some(value) = value {
            lines.push(Line::from(vec![
                Span::from(format!("{label}: ")).fg(Color::DarkGray),
                Span::from(value.to_string()),
            ]));
        }
    };
    field("description", package.description.as_deref());
    field("latest", package.latest_version.as_deref());
    field("homepage", package.homepage.as_deref());
    field("license", package.license.as_deref());
    field("origin", package.origin.as_deref());
    lines
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::from(" q quit · / search · Tab focus · ↑/↓ move ").fg(Color::DarkGray)];
    let status = app.status_line();
    if !status.is_empty() {
        spans.push(Span::from("· ").fg(Color::DarkGray));
        spans.push(Span::from(status).fg(Color::Yellow));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}
