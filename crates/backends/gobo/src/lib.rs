//! GoboLinux Compile backend for snowcone.
//!
//! GoboLinux keeps no package database - the `/Programs` tree is it: one
//! directory per program, one subdirectory per version, `Current` pointing
//! at the active one, and metadata under `Resources/`. Listing and info
//! read that tree directly. `Compile` builds from recipes and
//! `RemoveProgram` deletes an install; both escalate through GoboLinux's
//! own sudo wrapper when they need root, so snowcone never prefixes an
//! elevation helper. Neither script has a dry-run, so `--dry-run` errors.

use std::fs;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use snowcone_core::{
    BackendFactory, Capabilities, Cmd, Detection, Elevator, Error, HostInfo, InstallState,
    ManagerKind, OpContext, Operation, Package, PackageManager, PackageRequest, Result,
    find_program,
};

const ID: &str = "gobo";
const PROGRAMS: &[&str] = &["Compile"];

pub fn factory() -> Box<dyn BackendFactory> {
    Box::new(Factory)
}

struct Factory;

impl BackendFactory for Factory {
    fn id(&self) -> &'static str {
        ID
    }

    fn detect(&self, _host: &HostInfo) -> Detection {
        match PROGRAMS.iter().find_map(|program| find_program(program)) {
            Some(program) => Detection::Available { program },
            None => Detection::Unavailable {
                reason: format!("`{}` not found on PATH", PROGRAMS[0]),
            },
        }
    }

    fn create(&self, host: &HostInfo) -> Result<Box<dyn PackageManager>> {
        let resolve =
            |name: &str| find_program(name).ok_or_else(|| Error::Unavailable(ID.to_string()));
        Ok(Box::new(Manager {
            compile: resolve("Compile")?,
            remove_program: resolve("RemoveProgram")?,
            programs_root: PathBuf::from("/Programs"),
            elevator: Elevator::detect(host),
        }))
    }
}

struct Manager {
    compile: PathBuf,
    remove_program: PathBuf,
    programs_root: PathBuf,
    elevator: Elevator,
}

impl Manager {
    /// CLI passthrough when no event consumer is attached, captured and
    /// streamed otherwise.
    async fn run(&self, cmd: Cmd, ctx: &OpContext) -> Result<()> {
        let output = match &ctx.events {
            Some(events) => cmd.capture(&self.elevator, Some(events)).await?,
            None => cmd.run_interactive(&self.elevator).await?,
        };
        output.require_success()?;
        Ok(())
    }

    fn no_dry_run(&self, operation: &str) -> Error {
        Error::Other(format!("{ID}: {operation} has no dry-run mode"))
    }
}

/// Recipes are addressed by name; picking one of the store's versions is
/// not part of this backend's contract.
fn reject_pins(requests: &[PackageRequest]) -> Result<()> {
    match requests.iter().find(|request| request.version.is_some()) {
        Some(pinned) => Err(Error::Other(format!(
            "{ID}: `{pinned}` pins a version, but Compile builds the recipe's current version"
        ))),
        None => Ok(()),
    }
}

#[async_trait]
impl PackageManager for Manager {
    fn id(&self) -> &'static str {
        ID
    }

    fn display_name(&self) -> &'static str {
        "GoboLinux Compile"
    }

    fn kind(&self) -> ManagerKind {
        ManagerKind::System
    }

    fn database_id(&self) -> &'static str {
        "gobo"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CORE
    }

    /// Gobo's scripts sudo themselves when they need root - snowcone never
    /// elevates them, but a credential prompt is still coming.
    fn needs_elevation(&self, operation: Operation) -> bool {
        matches!(
            operation,
            Operation::Install | Operation::Remove | Operation::Upgrade | Operation::Refresh
        )
    }

    async fn install(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        reject_pins(packages)?;
        if ctx.dry_run {
            return Err(self.no_dry_run("install"));
        }
        // Compile takes one recipe per invocation.
        for package in packages {
            let mut cmd = Cmd::new(&self.compile);
            if ctx.assume_yes {
                cmd = cmd.arg("--batch");
            }
            self.run(cmd.arg(&package.name), ctx).await?;
        }
        Ok(())
    }

    async fn remove(&self, packages: &[PackageRequest], ctx: &OpContext) -> Result<()> {
        if ctx.dry_run {
            return Err(self.no_dry_run("remove"));
        }
        for package in packages {
            self.run(Cmd::new(&self.remove_program).arg(&package.name), ctx)
                .await?;
        }
        Ok(())
    }

    async fn list_installed(&self) -> Result<Vec<Box<dyn Package>>> {
        Ok(read_programs_tree(&self.programs_root)?
            .into_iter()
            .map(|package| Box::new(package) as Box<dyn Package>)
            .collect())
    }

    async fn info(&self, name: &str) -> Result<Box<dyn Package>> {
        let mut package = read_programs_tree(&self.programs_root)?
            .into_iter()
            .find(|package| package.name == name)
            .ok_or_else(|| Error::NotFound(name.to_string()))?;
        // Per-version metadata lives under `Resources/`.
        if let Some(version) = &package.version {
            let resources = self
                .programs_root
                .join(&package.name)
                .join(version)
                .join("Resources");
            package.description = fs::read_to_string(resources.join("Description"))
                .ok()
                .as_deref()
                .and_then(parse_description);
            package.dependencies = fs::read_to_string(resources.join("Dependencies"))
                .ok()
                .as_deref()
                .and_then(parse_dependencies);
        }
        Ok(Box::new(package))
    }
}

/// Walk the `/Programs` tree: every directory is an installed program, its
/// subdirectories are versions (`Settings`, `Variable`, and the `Current`
/// link are bookkeeping, not versions), and `Current` names the active
/// version. Unreadable entries are skipped rather than failing the listing.
fn read_programs_tree(root: &Path) -> Result<Vec<GoboPackage>> {
    let mut packages = Vec::new();
    for entry in fs::read_dir(root)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(subs) = fs::read_dir(&path) else {
            continue;
        };
        let mut versions = Vec::new();
        for sub in subs.flatten() {
            let sub_name = sub.file_name().to_string_lossy().into_owned();
            if matches!(sub_name.as_str(), "Settings" | "Variable" | "Current") {
                continue;
            }
            if sub.file_type().is_ok_and(|kind| kind.is_dir()) {
                versions.push(sub_name);
            }
        }
        let current = fs::read_link(path.join("Current"))
            .ok()
            .and_then(|target| target.file_name().map(|v| v.to_string_lossy().into_owned()));
        let Some(version) = pick_version(versions, current.as_deref()) else {
            continue;
        };
        packages.push(GoboPackage {
            name,
            version: Some(version),
            state: InstallState::Installed,
            ..Default::default()
        });
    }
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(packages)
}

/// The active version: what `Current` points at when it exists, otherwise
/// the highest-sorting version directory.
fn pick_version(mut versions: Vec<String>, current: Option<&str>) -> Option<String> {
    if let Some(current) = current
        && versions.iter().any(|version| version == current)
    {
        return Some(current.to_string());
    }
    versions.sort();
    versions.pop()
}

/// `Resources/Description`: free text, folded into one line.
fn parse_description(text: &str) -> Option<String> {
    let folded = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!folded.is_empty()).then_some(folded)
}

/// `Resources/Dependencies`: one dependency per line as `Name [op version]`;
/// blank lines and `#` comments are skipped.
fn parse_dependencies(text: &str) -> Option<Vec<String>> {
    let dependencies: Vec<String> = text
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            line.split_whitespace().next().map(str::to_string)
        })
        .collect();
    (!dependencies.is_empty()).then_some(dependencies)
}

/// A package as the `/Programs` tree describes it.
#[derive(Debug, Default)]
pub struct GoboPackage {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub dependencies: Option<Vec<String>>,
    pub state: InstallState,
}

impl Package for GoboPackage {
    fn manager(&self) -> &str {
        ID
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    fn dependencies(&self) -> Option<Vec<String>> {
        self.dependencies.clone()
    }

    fn state(&self) -> InstallState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT_TREE: AtomicU32 = AtomicU32::new(0);

    /// A throwaway `/Programs`-shaped directory, removed on drop.
    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "snowcone-gobo-test-{}-{}",
                std::process::id(),
                NEXT_TREE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn reads_programs_tree() {
        let tree = TempTree::new();
        let root = &tree.root;
        fs::create_dir_all(root.join("Bash/5.1")).unwrap();
        fs::create_dir_all(root.join("Bash/5.2")).unwrap();
        fs::create_dir_all(root.join("Bash/Settings")).unwrap();
        std::os::unix::fs::symlink("5.1", root.join("Bash/Current")).unwrap();
        fs::create_dir_all(root.join("Zsh/0.1")).unwrap();
        fs::write(root.join("stray-file"), "x").unwrap();
        let packages = read_programs_tree(root).unwrap();
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "Bash");
        assert_eq!(packages[0].version.as_deref(), Some("5.1"));
        assert_eq!(packages[0].state, InstallState::Installed);
        assert_eq!(packages[1].name, "Zsh");
        assert_eq!(packages[1].version.as_deref(), Some("0.1"));
    }

    #[test]
    fn current_link_wins_version_pick() {
        assert_eq!(
            pick_version(vec!["5.1".into(), "5.2".into()], Some("5.1")),
            Some("5.1".to_string())
        );
        assert_eq!(
            pick_version(vec!["5.1".into(), "5.2".into()], None),
            Some("5.2".to_string())
        );
        assert_eq!(pick_version(Vec::new(), None), None);
    }

    #[test]
    fn parses_description_resource() {
        assert_eq!(
            parse_description("The GNU Bourne\nAgain SHell.\n").as_deref(),
            Some("The GNU Bourne Again SHell.")
        );
        assert_eq!(parse_description("  \n"), None);
    }

    #[test]
    fn parses_dependencies_resource() {
        let text = "\
# build-time notes
Glibc >= 2.30
Ncurses
";
        assert_eq!(
            parse_dependencies(text),
            Some(vec!["Glibc".to_string(), "Ncurses".to_string()])
        );
        assert_eq!(parse_dependencies("# nothing\n"), None);
    }

    #[test]
    fn rejects_version_pins() {
        assert!(reject_pins(&[PackageRequest::parse("Bash@5.2")]).is_err());
        assert!(reject_pins(&[PackageRequest::parse("Bash")]).is_ok());
    }
}
