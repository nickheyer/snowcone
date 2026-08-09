//! Terminal output: plain tables for humans, JSON for machines.

use std::io::Write;

use snowcone_core::PackageSummary;

use crate::commands::ManagerStatus;

pub fn json<T: serde::Serialize>(value: &T) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer_pretty(&mut handle, value)?;
    writeln!(handle)?;
    Ok(())
}

pub fn packages(items: &[PackageSummary], as_json: bool) -> anyhow::Result<()> {
    if as_json {
        return json(&items);
    }
    if items.is_empty() {
        println!("no packages found");
        return Ok(());
    }
    let manager_width = column_width("MANAGER", items.iter().map(|p| p.manager.as_str()));
    let name_width = column_width("NAME", items.iter().map(|p| p.name.as_str()));
    let version_width = column_width(
        "VERSION",
        items.iter().map(|p| p.version.as_deref().unwrap_or("-")),
    );
    println!(
        "{:manager_width$}  {:name_width$}  {:version_width$}  DESCRIPTION",
        "MANAGER", "NAME", "VERSION"
    );
    for package in items {
        println!(
            "{:manager_width$}  {:name_width$}  {:version_width$}  {}",
            package.manager,
            package.name,
            package.version.as_deref().unwrap_or("-"),
            truncate(package.description.as_deref().unwrap_or(""), 70),
        );
    }
    Ok(())
}

pub fn details(package: &PackageSummary) {
    println!(
        "{} {} [{}]",
        package.name,
        package.version.as_deref().unwrap_or("-"),
        package.manager
    );
    println!("  state: {}", package.state);
    let field = |label: &str, value: &Option<String>| {
        if let Some(value) = value {
            println!("  {label}: {value}");
        }
    };
    field("description", &package.description);
    field("latest version", &package.latest_version);
    field("homepage", &package.homepage);
    field("license", &package.license);
    field("architecture", &package.architecture);
    field("origin", &package.origin);
    if let Some(size) = package.installed_size {
        println!("  installed size: {}", human_size(size));
    }
    if let Some(size) = package.download_size {
        println!("  download size: {}", human_size(size));
    }
    if let Some(dependencies) = &package.dependencies {
        println!("  dependencies: {}", dependencies.join(", "));
    }
}

pub fn managers(rows: &[ManagerStatus], as_json: bool) -> anyhow::Result<()> {
    if as_json {
        return json(&rows);
    }
    if rows.is_empty() {
        println!("no backends registered yet — backend crates are the next milestone");
        return Ok(());
    }
    let id_width = column_width("ID", rows.iter().map(|r| r.id.as_str()));
    let kind_width = column_width(
        "KIND",
        rows.iter().map(|r| r.kind.as_deref().unwrap_or("-")),
    );
    let db_width = column_width(
        "DATABASE",
        rows.iter().map(|r| r.database.as_deref().unwrap_or("-")),
    );
    println!(
        "{:id_width$}  {:kind_width$}  {:db_width$}  {:11}  DETAIL",
        "ID", "KIND", "DATABASE", "STATUS"
    );
    for row in rows {
        let detail = if row.capabilities.is_empty() {
            row.detail.clone()
        } else {
            format!("{} — {}", row.detail, row.capabilities.join(", "))
        };
        let status = if row.primary {
            "primary"
        } else if row.available {
            "available"
        } else {
            "unavailable"
        };
        println!(
            "{:id_width$}  {:kind_width$}  {:db_width$}  {status:11}  {detail}",
            row.id,
            row.kind.as_deref().unwrap_or("-"),
            row.database.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

fn column_width<'a>(header: &str, values: impl Iterator<Item = &'a str>) -> usize {
    values
        .map(str::len)
        .chain(std::iter::once(header.len()))
        .max()
        .unwrap_or(header.len())
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}
