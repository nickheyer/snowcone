//! Overlays: the confirmation modal (mutations, task cancellation, quit
//! guard) and the help overlay generated from the keymap.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use super::keys;
use super::pool::MutationPlan;
use super::tasks::TaskId;

pub enum Pending {
    /// One plan per database; confirmed as a unit, run FIFO.
    Mutations(Vec<MutationPlan>),
    /// Killing a running captured mutation mid-transaction.
    CancelTask(TaskId, String),
    /// Quit while a mutation is still running.
    Quit,
}

pub struct ConfirmState {
    pub pending: Pending,
    /// Default is No - destructive actions take a deliberate keystroke.
    pub yes_selected: bool,
}

impl ConfirmState {
    pub fn new(pending: Pending) -> Self {
        Self {
            pending,
            yes_selected: false,
        }
    }

    fn title(&self) -> &'static str {
        match &self.pending {
            Pending::Mutations(_) => " Confirm ",
            Pending::CancelTask(..) => " Cancel task ",
            Pending::Quit => " Quit? ",
        }
    }

    fn body(&self) -> Vec<Line<'static>> {
        match &self.pending {
            Pending::Mutations(plans) => {
                let mut lines = Vec::new();
                const SHOWN: usize = 6;
                for plan in plans.iter().take(SHOWN) {
                    lines.push(Line::from(vec![
                        Span::from("• ").fg(Color::Cyan),
                        Span::from(plan.title.clone()).bold(),
                    ]));
                    lines.push(
                        Line::from(format!(
                            "   [{}] · runs {}",
                            plan.database,
                            plan.mode.describe()
                        ))
                        .fg(Color::DarkGray),
                    );
                    if plan.needs_elevation {
                        lines.push(
                            Line::from("   will prompt for credentials").fg(Color::Yellow),
                        );
                    }
                }
                if plans.len() > SHOWN {
                    lines.push(Line::from(format!("   … and {} more", plans.len() - SHOWN)));
                }
                lines
            }
            Pending::CancelTask(_, title) => vec![
                Line::from(title.clone()).bold(),
                Line::from("Cancelling kills the tool mid-transaction.").fg(Color::Yellow),
            ],
            Pending::Quit => vec![
                Line::from("A mutation is still running.").bold(),
                Line::from("Quitting kills it mid-transaction.").fg(Color::Yellow),
            ],
        }
    }
}

pub fn draw_confirm(frame: &mut Frame, state: &ConfirmState) {
    let body = state.body();
    let height = (body.len() as u16 + 4).min(frame.area().height.saturating_sub(2));
    let area = centered(frame.area(), 64, height);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(state.title())
        .border_style(Style::new().fg(Color::Yellow));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [body_area, buttons_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
    frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), body_area);

    let button = |label: &'static str, selected: bool| {
        let style = if selected {
            Style::new().add_modifier(Modifier::REVERSED).bold()
        } else {
            Style::new().fg(Color::DarkGray)
        };
        Span::styled(label, style)
    };
    let buttons = Line::from(vec![
        Span::from("      "),
        button("[ No ]", !state.yes_selected),
        Span::from("      "),
        button("[ Yes ]", state.yes_selected),
    ])
    .centered();
    frame.render_widget(Paragraph::new(buttons), buttons_area);
}

pub fn draw_help(frame: &mut Frame, scroll: u16) {
    let area = centered_pct(frame.area(), 70, 80);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Help - ? or Esc to close ")
        .border_style(Style::new().fg(Color::Cyan));
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (section, entries) in keys::help_sections() {
        lines.push(Line::from(Span::from(*section).bold().fg(Color::Cyan)));
        for (key, what) in *entries {
            lines.push(Line::from(vec![
                Span::from(format!("  {key:<24}")).fg(Color::Yellow),
                Span::from(*what),
            ]));
        }
        lines.push(Line::from(""));
    }
    let max_scroll = (lines.len() as u16).saturating_sub(area.height.saturating_sub(2));
    let paragraph = Paragraph::new(lines)
        .block(block)
        .scroll((scroll.min(max_scroll), 0));
    frame.render_widget(paragraph, area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

fn centered_pct(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    centered(
        area,
        area.width * pct_x / 100,
        area.height * pct_y / 100,
    )
}
