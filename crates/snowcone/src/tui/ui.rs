//! Top-level rendering: tab bar, per-tab dispatch, footer, overlays, and
//! the small helpers every tab shares.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use snowcone_core::InstallState;

use super::app::{App, InfoEntry, InputTarget, Mode, Severity, Tab};
use super::packages::{PackageList, key_of};
use super::{modal, tabs};

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [tab_bar, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_tab_bar(frame, app, tab_bar);
    match app.tab {
        Tab::Search => tabs::search::draw(frame, app, body),
        Tab::Installed => tabs::installed::draw(frame, app, body),
        Tab::Outdated => tabs::outdated::draw(frame, app, body),
        Tab::Managers => tabs::managers::draw(frame, app, body),
        Tab::Tasks => tabs::tasks::draw(frame, app, body),
    }
    draw_footer(frame, app, footer);

    match &app.mode {
        Mode::Help { scroll } => modal::draw_help(frame, *scroll),
        Mode::Confirm(state) => modal::draw_confirm(frame, state),
        _ => {}
    }
}

fn tab_badge(app: &App, tab: Tab) -> Option<String> {
    match tab {
        Tab::Search if app.search.in_flight.is_some() => Some(spinner(app.tick).to_string()),
        Tab::Installed if app.installed.is_loading() => Some(spinner(app.tick).to_string()),
        Tab::Outdated if app.outdated.is_loading() => Some(spinner(app.tick).to_string()),
        Tab::Tasks => {
            let running = app.tasks.running_count();
            (running > 0).then(|| format!("{running}{}", spinner(app.tick)))
        }
        _ => None,
    }
}

fn draw_tab_bar(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![
        Span::from(" snowcone ").bold().fg(Color::Cyan),
        Span::from("│ ").fg(Color::DarkGray),
    ];
    for tab in Tab::ALL {
        let selected = app.tab == tab;
        let label = match tab_badge(app, tab) {
            Some(badge) => format!("[{}] {} {badge} ", tab.index() + 1, tab.title()),
            None => format!("[{}] {} ", tab.index() + 1, tab.title()),
        };
        let style = if selected {
            Style::new().bold().fg(Color::Cyan)
        } else {
            Style::new().fg(Color::DarkGray)
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::from(" "));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn hints(app: &App) -> &'static str {
    match &app.mode {
        Mode::Input(_) => " Enter apply · Esc back · Ctrl-U clear ",
        Mode::Confirm(_) => " y confirm · n / Esc cancel · ←/→ pick ",
        Mode::Help { .. } => " j/k scroll · Esc close ",
        Mode::Normal => match app.tab {
            Tab::Search => " / search · Space mark · i/d/u act · s sort · Enter details · ? help ",
            Tab::Installed => " / filter · Space mark · d remove · u upgrade · r reload · ? help ",
            Tab::Outdated => " / filter · u upgrade · U upgrade all · r reload · ? help ",
            Tab::Managers => " Space toggle · r re-probe · ? help ",
            Tab::Tasks => " Enter output · f follow · x cancel · C clear · ? help ",
        },
    }
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let right_line = match &app.status {
        Some((text, severity)) => {
            let color = match severity {
                Severity::Info => Color::Gray,
                Severity::Warn => Color::Yellow,
                Severity::Error => Color::Red,
            };
            Line::from(Span::from(text.clone()).fg(color)).right_aligned()
        }
        None => {
            let os = app.host.os.pretty_name.as_deref().unwrap_or("unknown");
            Line::from(
                Span::from(format!("{os} ({}) ", app.host.arch)).fg(Color::DarkGray),
            )
            .right_aligned()
        }
    };
    // The status gets the room it needs (up to ~2/3 of the row) so
    // messages don't truncate; hints absorb the rest.
    let wanted = right_line.width() as u16 + 1;
    let right_width = wanted.min(area.width * 2 / 3);
    let [left, right] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(right_width)]).areas(area);
    frame.render_widget(
        Paragraph::new(Span::from(hints(app)).fg(Color::DarkGray)),
        left,
    );
    frame.render_widget(Paragraph::new(right_line), right);
}

/// The filter bar shows while it is being edited or has content.
pub fn filter_bar_visible(app: &App, tab: Tab) -> bool {
    let editing = app.tab == tab && matches!(app.mode, Mode::Input(InputTarget::Filter));
    let list = match tab {
        Tab::Installed => &app.installed.list,
        Tab::Outdated => &app.outdated.list,
        _ => return false,
    };
    editing || !list.filter.is_empty()
}

pub fn draw_filter_bar(frame: &mut Frame, app: &App, tab: Tab, area: Rect) {
    let editing = app.tab == tab && matches!(app.mode, Mode::Input(InputTarget::Filter));
    let list = match tab {
        Tab::Installed => &app.installed.list,
        Tab::Outdated => &app.outdated.list,
        _ => return,
    };
    let block = Block::bordered()
        .title(" Filter (/) ")
        .border_style(border_style(editing));
    frame.render_widget(Paragraph::new(list.filter.as_str()).block(block), area);
    if editing {
        frame.set_cursor_position((area.x + 1 + list.filter.chars().count() as u16, area.y + 1));
    }
}

pub fn detail_height(app: &App, area: Rect) -> u16 {
    if app.detail_expanded {
        area.height / 2
    } else {
        9.min(area.height / 3)
    }
}

/// Detail pane for the package tabs: the row's summary immediately,
/// enriched with `info()` results once the cache has them.
pub fn draw_detail(frame: &mut Frame, app: &App, tab: &Tab, area: Rect) {
    let list: &PackageList = match tab {
        Tab::Search => &app.search.list,
        Tab::Installed => &app.installed.list,
        Tab::Outdated => &app.outdated.list,
        _ => return,
    };
    let block = Block::bordered()
        .title(if app.detail_expanded {
            " Details (Enter collapses) "
        } else {
            " Details (Enter expands) "
        })
        .border_style(border_style(false));
    let Some(package) = list.selected() else {
        frame.render_widget(
            Paragraph::new("Select a package to see its metadata.")
                .style(Style::new().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return;
    };

    let entry = app.info_cache.get(&key_of(package));
    // Prefer enriched fields when the info call brought them back.
    let rich = match entry {
        Some(InfoEntry::Loaded(summary)) => Some(summary.as_ref()),
        _ => None,
    };
    let field = |label: &str, value: Option<String>, lines: &mut Vec<Line<'static>>| {
        if let Some(value) = value
            && !value.is_empty()
        {
            lines.push(Line::from(vec![
                Span::from(format!("{label}: ")).fg(Color::DarkGray),
                Span::from(value),
            ]));
        }
    };

    let mut lines = vec![Line::from(vec![
        Span::from(package.name.clone()).bold(),
        Span::from(" "),
        Span::from(
            rich.and_then(|summary| summary.version.clone())
                .or_else(|| package.version.clone())
                .unwrap_or_else(|| "-".to_string()),
        ),
        Span::from(format!("  [{}]", package.manager)).fg(Color::DarkGray),
        Span::from(format!("  {}", package.state)).fg(state_color(package.state)),
    ])];
    let pick = |summary: fn(&snowcone_core::PackageSummary) -> Option<String>| {
        rich.and_then(summary).or_else(|| summary(package))
    };
    field(
        "description",
        pick(|summary| summary.description.clone()),
        &mut lines,
    );
    field(
        "latest",
        pick(|summary| summary.latest_version.clone()),
        &mut lines,
    );
    field(
        "homepage",
        pick(|summary| summary.homepage.clone()),
        &mut lines,
    );
    field("license", pick(|summary| summary.license.clone()), &mut lines);
    field("origin", pick(|summary| summary.origin.clone()), &mut lines);
    field(
        "architecture",
        pick(|summary| summary.architecture.clone()),
        &mut lines,
    );
    field(
        "installed size",
        rich.and_then(|summary| summary.installed_size)
            .or(package.installed_size)
            .map(human_size),
        &mut lines,
    );
    field(
        "download size",
        rich.and_then(|summary| summary.download_size)
            .or(package.download_size)
            .map(human_size),
        &mut lines,
    );
    if let Some(dependencies) = rich
        .and_then(|summary| summary.dependencies.clone())
        .or_else(|| package.dependencies.clone())
    {
        let shown: Vec<&str> = dependencies.iter().take(8).map(String::as_str).collect();
        let suffix = if dependencies.len() > shown.len() {
            format!(" +{} more", dependencies.len() - shown.len())
        } else {
            String::new()
        };
        field(
            &format!("depends ({})", dependencies.len()),
            Some(format!("{}{suffix}", shown.join(", "))),
            &mut lines,
        );
    }
    match entry {
        Some(InfoEntry::Loading) => {
            lines.push(Line::from(format!("{} fetching details…", spinner(app.tick))).italic())
        }
        Some(InfoEntry::Failed(error)) => {
            lines.push(Line::from(format!("info failed: {error}")).fg(Color::DarkGray))
        }
        _ => {}
    }
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(block),
        area,
    );
}

pub fn border_style(focused: bool) -> Style {
    if focused {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().fg(Color::DarkGray)
    }
}

pub fn state_color(state: InstallState) -> Color {
    match state {
        InstallState::Installed => Color::Green,
        InstallState::Upgradable => Color::Yellow,
        InstallState::Available => Color::DarkGray,
        InstallState::Unknown => Color::DarkGray,
    }
}

pub fn state_span(state: InstallState) -> Span<'static> {
    Span::styled(state.to_string(), Style::new().fg(state_color(state)))
}

pub fn spinner(tick: u64) -> char {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    FRAMES[(tick % FRAMES.len() as u64) as usize]
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn human_duration(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}
