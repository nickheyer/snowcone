//! Search-result relevance, shared by the CLI's `snow search` ordering and
//! the TUI Search tab's default sort.

/// Rank of `name` against `query`: lower sorts first. Exact match beats
/// prefix beats substring beats everything else; ties break toward shorter
/// names (the query covers more of them).
pub fn rank(name: &str, query: &str) -> (u8, usize) {
    let name = name.to_lowercase();
    let query = query.to_lowercase();
    let class = if name == query {
        0
    } else if name.starts_with(&query) {
        1
    } else if name.contains(&query) {
        2
    } else {
        3
    };
    (class, name.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_beats_prefix_beats_substring_beats_rest() {
        let mut names = vec!["neovim-git", "vim", "vim-airline", "gvim", "emacs"];
        names.sort_by_key(|name| rank(name, "vim"));
        assert_eq!(names, vec!["vim", "vim-airline", "gvim", "neovim-git", "emacs"]);
    }

    #[test]
    fn rank_is_case_insensitive() {
        assert_eq!(rank("RipGrep", "ripgrep").0, 0);
    }

    #[test]
    fn shorter_names_win_ties() {
        assert!(rank("vim-a", "vim") < rank("vim-airline", "vim"));
    }
}
