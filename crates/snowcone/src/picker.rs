//! yay-style pick menu for ambiguous CLI operations: a numbered candidate
//! list printed on the controlling terminal, one choice read back. Talks
//! to `/dev/tty` directly so piped stdin/stdout never break the menu.

use std::io::{BufRead, BufReader, Write};

use anyhow::bail;
use snowcone_core::{InstallState, PackageSummary};

/// Ask the user to pick one candidate, returning its index. Candidates
/// arrive in database order with the elected manager per database, so
/// index 0 is the bare-Enter default and the `-y` auto-pick.
pub async fn pick(
    headline: String,
    candidates: &[PackageSummary],
    assume_yes: bool,
) -> anyhow::Result<usize> {
    if assume_yes {
        eprintln!("snow: {headline}; -y picks {}", candidates[0].manager);
        return Ok(0);
    }
    let rows = render(candidates);
    let managers: Vec<String> = candidates
        .iter()
        .map(|candidate| candidate.manager.clone())
        .collect();
    // Blocking tty I/O off the async runtime.
    tokio::task::spawn_blocking(move || ask(&headline, &rows, &managers)).await?
}

fn ask(headline: &str, rows: &[String], managers: &[String]) -> anyhow::Result<usize> {
    let count = rows.len();
    let tty = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    {
        Ok(tty) => tty,
        Err(_) => bail!(
            "{headline} ({}) and there is no terminal to ask on - \
             pick one with --manager, or pass -y to take {}",
            managers.join(", "),
            managers[0],
        ),
    };
    let mut reader = BufReader::new(tty.try_clone()?);
    let mut tty = tty;
    writeln!(tty, ":: {headline}:")?;
    for row in rows {
        writeln!(tty, "{row}")?;
    }
    loop {
        write!(tty, "==> pick 1-{count} (Enter = 1, q aborts): ")?;
        tty.flush()?;
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            bail!("terminal closed before a choice was made");
        }
        let line = line.trim();
        if line.is_empty() {
            return Ok(0);
        }
        if line.eq_ignore_ascii_case("q") {
            bail!("aborted");
        }
        match line.parse::<usize>() {
            Ok(choice) if (1..=count).contains(&choice) => return Ok(choice - 1),
            _ => writeln!(tty, "   `{line}` is not 1-{count}")?,
        }
    }
}

fn render(candidates: &[PackageSummary]) -> Vec<String> {
    let width = |values: &mut dyn Iterator<Item = usize>| values.max().unwrap_or(0);
    let manager_width = width(&mut candidates.iter().map(|c| c.manager.len()));
    let version_width = width(
        &mut candidates
            .iter()
            .map(|c| c.version.as_deref().unwrap_or("-").len()),
    );
    candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            let marker = match candidate.state {
                InstallState::Installed => "[installed]",
                InstallState::Upgradable => "[upgradable]",
                InstallState::Available | InstallState::Unknown => "",
            };
            let description = candidate.description.as_deref().unwrap_or("");
            format!(
                "  {:>3}  {:manager_width$}  {:version_width$}  {marker:<12}  {}",
                index + 1,
                candidate.manager,
                candidate.version.as_deref().unwrap_or("-"),
                truncate(description, 60),
            )
            .trim_end()
            .to_string()
        })
        .collect()
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}
