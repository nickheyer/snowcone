//! The central execution-mode policy: which managers may run mutations
//! captured inside the TUI, and which take over the terminal.
//!
//! Captured mode pipes the child's output into the Tasks pane with stdin
//! closed and `assume_yes` forced, so it is only safe for tools that never
//! prompt - not for credentials, not for confirmation. Everything else
//! suspends the TUI and runs on the real terminal. One table, explicit
//! ids, no inference from manager kind; an unlisted manager gets the safe
//! default (Interactive - worst case is an unnecessary suspension, never
//! an invisible sudo prompt).

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
    ("go", ExecMode::Captured),
    ("nix", ExecMode::Captured),
    ("npm", ExecMode::Captured),
    ("pip", ExecMode::Captured),
    ("pipx", ExecMode::Captured),
    // System tools elevated by snowcone (sudo prompts on /dev/tty):
    ("apt", ExecMode::Interactive),
    ("aptitude", ExecMode::Interactive),
    // AUR helpers that invoke sudo themselves mid-run (invisible under
    // capture):
    ("aura", ExecMode::Interactive),
    ("paru", ExecMode::Interactive),
    ("pikaur", ExecMode::Interactive),
    ("trizen", ExecMode::Interactive),
    ("yay", ExecMode::Interactive),
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
                registry.factories().iter().any(|factory| factory.id() == *id),
                "policy names unknown backend: {id}"
            );
        }
    }

    #[test]
    fn unlisted_managers_default_to_interactive() {
        assert_eq!(exec_mode("dnf"), ExecMode::Interactive);
        assert_eq!(exec_mode("not-a-backend"), ExecMode::Interactive);
    }

    #[test]
    fn userspace_managers_are_captured() {
        assert_eq!(exec_mode("npm"), ExecMode::Captured);
        assert_eq!(exec_mode("pip"), ExecMode::Captured);
    }
}
