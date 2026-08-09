//! Host introspection: os-release, architecture, privileges, and locating
//! backend executables.

use std::path::{Path, PathBuf};

/// Parsed `/etc/os-release` (or `/usr/lib/os-release`).
#[derive(Clone, Debug, Default)]
pub struct OsRelease {
    pub id: Option<String>,
    pub id_like: Vec<String>,
    pub name: Option<String>,
    pub pretty_name: Option<String>,
    pub version_id: Option<String>,
}

impl OsRelease {
    pub fn load() -> Self {
        ["/etc/os-release", "/usr/lib/os-release"]
            .iter()
            .find_map(|path| std::fs::read_to_string(path).ok())
            .map(|contents| Self::parse(&contents))
            .unwrap_or_default()
    }

    pub fn parse(contents: &str) -> Self {
        let mut os = Self::default();
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().trim_matches('"').trim_matches('\'');
            match key.trim() {
                "ID" => os.id = Some(value.to_string()),
                "ID_LIKE" => {
                    os.id_like = value.split_whitespace().map(str::to_string).collect();
                }
                "NAME" => os.name = Some(value.to_string()),
                "PRETTY_NAME" => os.pretty_name = Some(value.to_string()),
                "VERSION_ID" => os.version_id = Some(value.to_string()),
                _ => {}
            }
        }
        os
    }

    /// True when this distro is, or descends from, `id` (checks `ID` and
    /// `ID_LIKE`) — how backends decide "am I the native manager here".
    pub fn is_like(&self, id: &str) -> bool {
        self.id.as_deref().is_some_and(|own| own.eq_ignore_ascii_case(id))
            || self.id_like.iter().any(|like| like.eq_ignore_ascii_case(id))
    }
}

/// Everything a [`BackendFactory`](crate::BackendFactory) may probe during
/// discovery.
#[derive(Clone, Debug)]
pub struct HostInfo {
    pub os: OsRelease,
    /// Target architecture snow was built for (`x86_64`, `aarch64`, …).
    pub arch: &'static str,
    /// Effective uid is root; mutating system operations then need no
    /// elevation helper.
    pub is_root: bool,
}

impl HostInfo {
    pub fn detect() -> Self {
        Self {
            os: OsRelease::load(),
            arch: std::env::consts::ARCH,
            is_root: unsafe { libc::geteuid() } == 0,
        }
    }
}

/// Locate a program on `PATH`, falling back to the sbin directories that
/// non-root user PATHs often omit.
pub fn find_program(name: &str) -> Option<PathBuf> {
    which::which(name).ok().or_else(|| {
        ["/usr/local/sbin", "/usr/sbin", "/sbin"]
            .iter()
            .map(|dir| Path::new(dir).join(name))
            .find(|candidate| is_executable(candidate))
    })
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_os_release() {
        let os = OsRelease::parse(
            r#"
NAME="Arch Linux"
PRETTY_NAME="Arch Linux"
ID=arch
# a comment
VERSION_ID=20260801.0.404078
"#,
        );
        assert_eq!(os.id.as_deref(), Some("arch"));
        assert_eq!(os.pretty_name.as_deref(), Some("Arch Linux"));
        assert!(os.id_like.is_empty());
        assert!(os.is_like("arch"));
        assert!(!os.is_like("debian"));
    }

    #[test]
    fn id_like_counts_as_like() {
        let os = OsRelease::parse("ID=ubuntu\nID_LIKE=debian\n");
        assert!(os.is_like("debian"));
        assert!(os.is_like("ubuntu"));
    }
}
