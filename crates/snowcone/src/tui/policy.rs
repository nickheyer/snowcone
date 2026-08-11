//! The central execution-mode policy: which managers may run mutations
//! captured inside the TUI, and which take over the terminal.
//!
//! Captured mode pipes the child's output into the Tasks pane with stdin
//! closed and `assume_yes` forced. It is safe for a tool when nothing in
//! the run can stop to ask a question:
//!
//! - it never prompts (userspace tools with a non-interactive contract), or
//! - its only prompt is sudo's password, which the TUI now satisfies up
//!   front: a modal collects the password, `Elevator::hold_with_password`
//!   validates it and keeps the timestamp warm, and captured commands run
//!   `sudo -n` so a cold cache fails cleanly instead of painting a prompt
//!   over the TUI. That covers both snowcone-elevated tools and
//!   self-sudoing ones (AUR helpers) - their inner `sudo` hits the same
//!   warm timestamp.
//!
//! Interactive (suspend the TUI) remains for tools that authorize through
//! polkit (its agent owns the prompt; sudo warmth is irrelevant) and tools
//! that genuinely converse on the terminal. One table, explicit ids, no
//! inference from manager kind; an unlisted manager gets the safe default
//! (Interactive - worst case is an unnecessary suspension, never a hung
//! or invisible prompt).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecMode {
    /// Background task, output streamed into the Tasks pane.
    Captured,
    /// Suspend the TUI; the tool owns the terminal until it exits.
    Interactive,
}

impl ExecMode {
    pub fn describe(self) -> &'static str {
        match self {
            ExecMode::Captured => "in the background (output in the Tasks tab)",
            ExecMode::Interactive => "on the terminal (TUI suspends until it finishes)",
        }
    }
}

const POLICY: &[(&str, ExecMode)] = &[
    // Userspace tools that never prompt and never elevate:
    ("brew", ExecMode::Captured),
    ("bun", ExecMode::Captured),
    ("bundler", ExecMode::Captured),
    ("cabal", ExecMode::Captured),
    ("cargo", ExecMode::Captured),
    ("cargo-binstall", ExecMode::Captured),
    ("clojure", ExecMode::Captured),
    ("composer", ExecMode::Captured),
    ("conda", ExecMode::Captured),
    ("coursier", ExecMode::Captured),
    ("cpan", ExecMode::Captured),
    ("cpanm", ExecMode::Captured),
    ("cpm", ExecMode::Captured),
    ("deno", ExecMode::Captured),
    ("dotnet", ExecMode::Captured),
    ("dub", ExecMode::Captured),
    ("easybuild", ExecMode::Captured),
    ("fpm", ExecMode::Captured),
    ("gem", ExecMode::Captured),
    ("go", ExecMode::Captured),
    ("gradle", ExecMode::Captured),
    ("guix", ExecMode::Captured),
    ("hatch", ExecMode::Captured),
    ("haxelib", ExecMode::Captured),
    ("helm", ExecMode::Captured),
    ("ivy", ExecMode::Captured),
    ("julia", ExecMode::Captured),
    ("leiningen", ExecMode::Captured),
    ("luarocks", ExecMode::Captured),
    ("mamba", ExecMode::Captured),
    ("maven", ExecMode::Captured),
    ("mix", ExecMode::Captured),
    ("mpm", ExecMode::Captured),
    ("nimble", ExecMode::Captured),
    ("nix", ExecMode::Captured),
    ("npm", ExecMode::Captured),
    ("opam", ExecMode::Captured),
    ("paket", ExecMode::Captured),
    ("pdm", ExecMode::Captured),
    ("pip", ExecMode::Captured),
    ("pipx", ExecMode::Captured),
    ("pixi", ExecMode::Captured),
    ("pnpm", ExecMode::Captured),
    ("poetry", ExecMode::Captured),
    ("pub", ExecMode::Captured),
    ("quicklisp", ExecMode::Captured),
    ("r", ExecMode::Captured),
    ("raco", ExecMode::Captured),
    ("rebar3", ExecMode::Captured),
    ("sbt", ExecMode::Captured),
    ("shards", ExecMode::Captured),
    ("spack", ExecMode::Captured),
    ("stack", ExecMode::Captured),
    ("swiftpm", ExecMode::Captured),
    ("uv", ExecMode::Captured),
    ("vcpkg", ExecMode::Captured),
    ("vpm", ExecMode::Captured),
    ("xmake", ExecMode::Captured),
    ("yarn", ExecMode::Captured),
    ("0install", ExecMode::Captured),
    ("zig", ExecMode::Captured),
    // System tools elevated by snowcone through sudo, with a reliable
    // yes-flag under `assume_yes`. Safe captured because the TUI holds a
    // warm credential session and elevates with `sudo -n`:
    ("apk", ExecMode::Captured),
    ("apt", ExecMode::Captured),
    ("apt-rpm", ExecMode::Captured),
    ("aptitude", ExecMode::Captured),
    ("dnf", ExecMode::Captured),
    ("dpkg", ExecMode::Captured),
    ("emerge", ExecMode::Captured),
    ("eopkg", ExecMode::Captured),
    ("luet", ExecMode::Captured),
    ("nala", ExecMode::Captured),
    ("opkg", ExecMode::Captured),
    ("pacman", ExecMode::Captured),
    ("pisi", ExecMode::Captured),
    ("pkgtools", ExecMode::Captured),
    ("prt-get", ExecMode::Captured),
    ("rpm", ExecMode::Captured),
    ("slackpkg", ExecMode::Captured),
    ("slapt-get", ExecMode::Captured),
    ("slpkg", ExecMode::Captured),
    ("snap", ExecMode::Captured),
    ("stow", ExecMode::Captured),
    ("swupd", ExecMode::Captured),
    ("tlmgr", ExecMode::Captured),
    ("transactional-update", ExecMode::Captured),
    ("urpmi", ExecMode::Captured),
    ("xbps", ExecMode::Captured),
    ("yum", ExecMode::Captured),
    ("zypper", ExecMode::Captured),
    // Tools that escalate themselves mid-run via sudo (makepkg refuses
    // root); their inner sudo hits the TUI's warm timestamp:
    ("aura", ExecMode::Captured),
    ("eepm", ExecMode::Captured),
    ("kiss", ExecMode::Captured),
    ("makedeb", ExecMode::Captured),
    ("pacstall", ExecMode::Captured),
    ("paru", ExecMode::Captured),
    ("pikaur", ExecMode::Captured),
    ("trizen", ExecMode::Captured),
    ("yay", ExecMode::Captured),
    // polkit-authorized tools: the polkit agent owns the prompt and a
    // warm sudo timestamp is irrelevant, so the terminal must be free:
    ("flatpak", ExecMode::Interactive),
    ("packagekit", ExecMode::Interactive),
    ("pamac", ExecMode::Interactive),
    ("rpm-ostree", ExecMode::Interactive),
    // Genuinely conversational or unverified-interactive tools (version
    // pickers, build-time questions, GUI-adjacent wrappers) - and
    // everything unlisted - suspend:
    ("apx", ExecMode::Interactive),
    ("bauh", ExecMode::Interactive),
    ("cave", ExecMode::Interactive),
    ("gobo", ExecMode::Interactive),
    ("lunar", ExecMode::Interactive),
    ("netpkg", ExecMode::Interactive),
    ("petget", ExecMode::Interactive),
    ("sbopkg", ExecMode::Interactive),
    ("scratchpkg", ExecMode::Interactive),
    ("sorcery", ExecMode::Interactive),
    ("tce-load", ExecMode::Interactive),
];

pub fn exec_mode(manager_id: &str) -> ExecMode {
    POLICY
        .iter()
        .find(|(id, _)| *id == manager_id)
        .map(|(_, mode)| *mode)
        .unwrap_or(ExecMode::Interactive)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_duplicate_ids() {
        for (index, (id, _)) in POLICY.iter().enumerate() {
            assert!(
                !POLICY[index + 1..].iter().any(|(other, _)| other == id),
                "duplicate policy entry: {id}"
            );
        }
    }

    #[test]
    fn every_policy_id_is_a_registered_backend() {
        let mut registry = snowcone_core::Registry::new();
        snowcone_backends::register_all(&mut registry);
        for (id, _) in POLICY {
            assert!(
                registry
                    .factories()
                    .iter()
                    .any(|factory| factory.id() == *id),
                "policy names unknown backend: {id}"
            );
        }
    }

    #[test]
    fn unlisted_managers_default_to_interactive() {
        assert_eq!(exec_mode("not-a-backend"), ExecMode::Interactive);
    }

    #[test]
    fn userspace_managers_are_captured() {
        assert_eq!(exec_mode("npm"), ExecMode::Captured);
        assert_eq!(exec_mode("pip"), ExecMode::Captured);
    }

    #[test]
    fn sudo_elevated_system_managers_are_captured() {
        assert_eq!(exec_mode("apt"), ExecMode::Captured);
        assert_eq!(exec_mode("pacman"), ExecMode::Captured);
        assert_eq!(exec_mode("yay"), ExecMode::Captured);
    }

    #[test]
    fn polkit_managers_stay_interactive() {
        assert_eq!(exec_mode("flatpak"), ExecMode::Interactive);
        assert_eq!(exec_mode("packagekit"), ExecMode::Interactive);
        assert_eq!(exec_mode("rpm-ostree"), ExecMode::Interactive);
    }
}
