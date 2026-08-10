//! Package table model: filtering, sorting, marks, and selection -
//! everything the Search/Installed/Outdated tables share.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use ratatui::widgets::TableState;
use snowcone_core::{InstallState, PackageSummary};

use super::tasks::TaskId;

/// Identity of a package row across rebuilds: `PackageSummary` has no
/// `PartialEq`, and (manager, name) is what mutations key on anyway.
pub type PkgKey = (String, String);

pub fn key_of(package: &PackageSummary) -> PkgKey {
    (package.manager.clone(), package.name.clone())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    /// Best match for the current query first - the Search tab's default.
    Relevance,
    Name,
    Manager,
    Version,
    State,
}

impl SortKey {
    pub fn next(self) -> Self {
        match self {
            SortKey::Relevance => SortKey::Name,
            SortKey::Name => SortKey::Manager,
            SortKey::Manager => SortKey::Version,
            SortKey::Version => SortKey::State,
            SortKey::State => SortKey::Relevance,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SortKey::Relevance => "relevance",
            SortKey::Name => "name",
            SortKey::Manager => "manager",
            SortKey::Version => "version",
            SortKey::State => "state",
        }
    }
}

pub struct PackageList {
    rows: Vec<PackageSummary>,
    /// Filter+sort projection: indices into `rows`, in display order.
    visible: Vec<usize>,
    pub table: TableState,
    pub sort: SortKey,
    pub sort_desc: bool,
    pub filter: String,
    /// What relevance sorts against - the Search tab's query terms; empty
    /// everywhere else.
    pub query: String,
    pub marked: BTreeSet<PkgKey>,
}

impl PackageList {
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            visible: Vec::new(),
            table: TableState::default(),
            sort: SortKey::Name,
            sort_desc: false,
            filter: String::new(),
            query: String::new(),
            marked: BTreeSet::new(),
        }
    }

    pub fn set_rows(&mut self, rows: Vec<PackageSummary>) {
        // Capture against the OLD rows - the projection is stale the
        // moment `rows` is replaced.
        let selected_key = self.selected().map(key_of);
        self.rows = rows;
        // Marks on rows that no longer exist would silently target nothing.
        let keys: BTreeSet<PkgKey> = self.rows.iter().map(key_of).collect();
        self.marked.retain(|key| keys.contains(key));
        self.rebuild_with(selected_key);
    }

    pub fn extend_rows(&mut self, more: Vec<PackageSummary>) {
        self.rows.extend(more);
        self.rebuild();
    }

    pub fn update_rows(&mut self, mut update: impl FnMut(&mut PackageSummary)) {
        for row in &mut self.rows {
            update(row);
        }
        self.rebuild();
    }

    pub fn total_len(&self) -> usize {
        self.rows.len()
    }

    pub fn visible_len(&self) -> usize {
        self.visible.len()
    }

    pub fn visible_rows(&self) -> impl Iterator<Item = &PackageSummary> {
        self.visible.iter().map(|&index| &self.rows[index])
    }

    pub fn selected(&self) -> Option<&PackageSummary> {
        self.table
            .selected()
            .and_then(|index| self.visible.get(index))
            .and_then(|&index| self.rows.get(index))
    }

    /// Marked rows when any exist (in display order), else the selection.
    pub fn targets(&self) -> Vec<&PackageSummary> {
        if self.marked.is_empty() {
            return self.selected().into_iter().collect();
        }
        self.visible_rows()
            .filter(|row| self.marked.contains(&key_of(row)))
            .collect()
    }

    pub fn is_marked(&self, package: &PackageSummary) -> bool {
        self.marked.contains(&key_of(package))
    }

    pub fn toggle_mark(&mut self) {
        if let Some(selected) = self.selected() {
            let key = key_of(selected);
            if !self.marked.remove(&key) {
                self.marked.insert(key);
            }
            self.move_selection(1);
        }
    }

    /// Mark every visible row, or unmark them all when they already are.
    pub fn mark_all_visible(&mut self) {
        let keys: Vec<PkgKey> = self.visible_rows().map(key_of).collect();
        if keys.iter().all(|key| self.marked.contains(key)) {
            for key in &keys {
                self.marked.remove(key);
            }
        } else {
            self.marked.extend(keys);
        }
    }

    pub fn clear_marks(&mut self) -> bool {
        let had = !self.marked.is_empty();
        self.marked.clear();
        had
    }

    pub fn filter_push(&mut self, c: char) {
        self.filter.push(c);
        self.rebuild();
    }

    pub fn filter_pop(&mut self) {
        self.filter.pop();
        self.rebuild();
    }

    pub fn filter_clear(&mut self) -> bool {
        let had = !self.filter.is_empty();
        self.filter.clear();
        if had {
            self.rebuild();
        }
        had
    }

    pub fn cycle_sort(&mut self) {
        self.sort = self.sort.next();
        // Relevance means nothing without a query (Installed / Outdated).
        if self.sort == SortKey::Relevance && self.query.is_empty() {
            self.sort = self.sort.next();
        }
        self.rebuild();
    }

    pub fn toggle_sort_dir(&mut self) {
        self.sort_desc = !self.sort_desc;
        self.rebuild();
    }

    /// `▲`/`▼` for the column currently sorted by, empty otherwise.
    pub fn sort_indicator(&self, key: SortKey) -> &'static str {
        if self.sort != key {
            ""
        } else if self.sort_desc {
            " ▼"
        } else {
            " ▲"
        }
    }

    pub fn move_selection(&mut self, delta: i64) {
        let len = self.visible.len();
        if len == 0 {
            return;
        }
        let current = self.table.selected().unwrap_or(0) as i64;
        let next = (current + delta).rem_euclid(len as i64) as usize;
        self.table.select(Some(next));
    }

    pub fn page_selection(&mut self, delta: i64) {
        let len = self.visible.len();
        if len == 0 {
            return;
        }
        let current = self.table.selected().unwrap_or(0) as i64;
        let next = (current + delta).clamp(0, len as i64 - 1) as usize;
        self.table.select(Some(next));
    }

    pub fn select_home(&mut self) {
        if !self.visible.is_empty() {
            self.table.select(Some(0));
        }
    }

    pub fn select_end(&mut self) {
        if !self.visible.is_empty() {
            self.table.select(Some(self.visible.len() - 1));
        }
    }

    /// Recompute the filter+sort projection, keeping the selected row
    /// selected (by key) when it survives.
    fn rebuild(&mut self) {
        let selected_key = self.selected().map(key_of);
        self.rebuild_with(selected_key);
    }

    fn rebuild_with(&mut self, selected_key: Option<PkgKey>) {
        let needle = self.filter.to_lowercase();
        self.visible = (0..self.rows.len())
            .filter(|&index| matches_filter(&self.rows[index], &needle))
            .collect();
        self.visible.sort_by(|&a, &b| {
            let ordering = compare_rows(&self.rows[a], &self.rows[b], self.sort, &self.query);
            if self.sort_desc {
                ordering.reverse()
            } else {
                ordering
            }
        });
        let next = selected_key
            .and_then(|key| {
                self.visible
                    .iter()
                    .position(|&index| key_of(&self.rows[index]) == key)
            })
            .or_else(|| {
                self.table
                    .selected()
                    .map(|index| index.min(self.visible.len().saturating_sub(1)))
            })
            .filter(|_| !self.visible.is_empty())
            .or(if self.visible.is_empty() {
                None
            } else {
                Some(0)
            });
        self.table.select(next);
    }
}

fn matches_filter(row: &PackageSummary, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    row.name.to_lowercase().contains(needle)
        || row.manager.to_lowercase().contains(needle)
        || row
            .description
            .as_deref()
            .is_some_and(|description| description.to_lowercase().contains(needle))
        || row
            .origin
            .as_deref()
            .is_some_and(|origin| origin.to_lowercase().contains(needle))
}

fn compare_rows(a: &PackageSummary, b: &PackageSummary, sort: SortKey, query: &str) -> Ordering {
    match sort {
        SortKey::Relevance => crate::relevance::rank(&a.name, query)
            .cmp(&crate::relevance::rank(&b.name, query))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.manager.cmp(&b.manager)),
        SortKey::Name => (a.name.as_str(), a.manager.as_str()).cmp(&(&b.name, &b.manager)),
        SortKey::Manager => (a.manager.as_str(), a.name.as_str()).cmp(&(&b.manager, &b.name)),
        SortKey::Version => compare_versions(a.version.as_deref(), b.version.as_deref())
            .then_with(|| a.name.cmp(&b.name)),
        SortKey::State => state_rank(a.state)
            .cmp(&state_rank(b.state))
            .then_with(|| a.name.cmp(&b.name)),
    }
}

/// Actionable first.
fn state_rank(state: InstallState) -> u8 {
    match state {
        InstallState::Upgradable => 0,
        InstallState::Installed => 1,
        InstallState::Available => 2,
        InstallState::Unknown => 3,
    }
}

/// Numeric-chunk version comparison: `1.10.2` > `1.9.1`, `2.0-rc1` vs
/// `2.0` compares the suffix lexically. Unknown versions sort last.
pub fn compare_versions(a: Option<&str>, b: Option<&str>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => {
            let mut a_chunks = chunks(a);
            let mut b_chunks = chunks(b);
            loop {
                match (a_chunks.next(), b_chunks.next()) {
                    (None, None) => return Ordering::Equal,
                    (None, Some(_)) => return Ordering::Less,
                    (Some(_), None) => return Ordering::Greater,
                    (Some(x), Some(y)) => {
                        let both_numeric = x.bytes().all(|b| b.is_ascii_digit())
                            && y.bytes().all(|b| b.is_ascii_digit());
                        let ordering = if both_numeric {
                            compare_digits(x, y)
                        } else {
                            x.cmp(y)
                        };
                        if ordering != Ordering::Equal {
                            return ordering;
                        }
                    }
                }
            }
        }
    }
}

/// Alternating digit / non-digit runs.
fn chunks(version: &str) -> impl Iterator<Item = &str> {
    let bytes = version.as_bytes();
    let mut start = 0;
    std::iter::from_fn(move || {
        if start >= bytes.len() {
            return None;
        }
        let digit = bytes[start].is_ascii_digit();
        let mut end = start + 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() == digit {
            end += 1;
        }
        let chunk = &version[start..end];
        start = end;
        Some(chunk)
    })
}

/// Compare digit runs without parsing (no overflow on absurd versions).
fn compare_digits(a: &str, b: &str) -> Ordering {
    let a = a.trim_start_matches('0');
    let b = b.trim_start_matches('0');
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// Load lifecycle of the Installed / Outdated tabs. Rows stay visible
/// while stale (`NotLoaded` after an invalidation) - a reload replaces
/// them wholesale.
pub enum LoadState {
    NotLoaded,
    Loading(TaskId),
    Loaded,
    Failed(String),
}

pub struct ListTab {
    pub load: LoadState,
    pub epoch: u64,
    pub list: PackageList,
}

impl ListTab {
    pub fn new() -> Self {
        Self {
            load: LoadState::NotLoaded,
            epoch: 0,
            list: PackageList::new(),
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self.load, LoadState::Loading(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(
        manager: &str,
        name: &str,
        version: Option<&str>,
        state: InstallState,
    ) -> PackageSummary {
        PackageSummary {
            manager: manager.to_string(),
            name: name.to_string(),
            version: version.map(str::to_string),
            latest_version: None,
            description: None,
            homepage: None,
            license: None,
            architecture: None,
            origin: None,
            installed_size: None,
            download_size: None,
            dependencies: None,
            state,
        }
    }

    fn names(list: &PackageList) -> Vec<&str> {
        list.visible_rows().map(|row| row.name.as_str()).collect()
    }

    #[test]
    fn sorts_by_name_then_manager_by_default() {
        let mut list = PackageList::new();
        list.set_rows(vec![
            pkg("npm", "zsh", None, InstallState::Available),
            pkg("apt", "bat", None, InstallState::Available),
            pkg("apt", "zsh", None, InstallState::Available),
        ]);
        assert_eq!(names(&list), vec!["bat", "zsh", "zsh"]);
        assert_eq!(list.visible_rows().nth(1).unwrap().manager, "apt");
    }

    #[test]
    fn version_sort_is_numeric_aware() {
        assert_eq!(
            compare_versions(Some("1.10.0"), Some("1.9.9")),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions(Some("14.1.0-1"), Some("14.1.0-1")),
            Ordering::Equal
        );
        assert_eq!(compare_versions(Some("1.0"), None), Ordering::Less);
        assert_eq!(
            compare_versions(Some("2.0"), Some("2.0-rc1")),
            Ordering::Less
        );
    }

    #[test]
    fn relevance_sort_puts_exact_match_first() {
        let mut list = PackageList::new();
        list.sort = SortKey::Relevance;
        list.query = "vim".to_string();
        list.set_rows(vec![
            pkg("apt", "neovim", None, InstallState::Available),
            pkg("apt", "vim-airline", None, InstallState::Available),
            pkg("apt", "vim", None, InstallState::Available),
        ]);
        assert_eq!(names(&list), vec!["vim", "vim-airline", "neovim"]);
    }

    #[test]
    fn cycle_skips_relevance_without_a_query() {
        let mut list = PackageList::new();
        list.sort = SortKey::State;
        list.cycle_sort();
        assert_eq!(list.sort, SortKey::Name);
        list.query = "vim".to_string();
        list.sort = SortKey::State;
        list.cycle_sort();
        assert_eq!(list.sort, SortKey::Relevance);
    }

    #[test]
    fn state_sort_puts_actionable_first() {
        let mut list = PackageList::new();
        list.sort = SortKey::State;
        list.set_rows(vec![
            pkg("apt", "avail", None, InstallState::Available),
            pkg("apt", "upgr", None, InstallState::Upgradable),
            pkg("apt", "inst", None, InstallState::Installed),
        ]);
        assert_eq!(names(&list), vec!["upgr", "inst", "avail"]);
    }

    #[test]
    fn filter_matches_name_manager_description() {
        let mut list = PackageList::new();
        let mut described = pkg("apt", "bat", None, InstallState::Available);
        described.description = Some("A cat clone with wings".to_string());
        list.set_rows(vec![
            described,
            pkg("npm", "typescript", None, InstallState::Available),
        ]);
        list.filter_push('w');
        list.filter_push('i');
        assert_eq!(names(&list), vec!["bat"]);
        list.filter_clear();
        assert_eq!(list.visible_len(), 2);
    }

    #[test]
    fn selection_survives_a_sort_change_by_key() {
        let mut list = PackageList::new();
        list.set_rows(vec![
            pkg("apt", "aaa", Some("2"), InstallState::Available),
            pkg("apt", "bbb", Some("1"), InstallState::Available),
        ]);
        list.select_end();
        assert_eq!(list.selected().unwrap().name, "bbb");
        list.sort = SortKey::Version;
        list.toggle_sort_dir();
        list.toggle_sort_dir();
        assert_eq!(list.selected().unwrap().name, "bbb");
    }

    #[test]
    fn marks_toggle_and_target() {
        let mut list = PackageList::new();
        list.set_rows(vec![
            pkg("apt", "one", None, InstallState::Available),
            pkg("apt", "two", None, InstallState::Available),
        ]);
        assert_eq!(list.targets().len(), 1); // selection only
        list.toggle_mark(); // marks "one", advances
        assert_eq!(list.targets().len(), 1);
        list.toggle_mark(); // marks "two"
        assert_eq!(list.targets().len(), 2);
        list.mark_all_visible(); // all already marked -> unmark
        assert!(list.marked.is_empty());
        // Marks on vanished rows are dropped.
        list.toggle_mark();
        list.set_rows(vec![pkg("apt", "two", None, InstallState::Available)]);
        assert!(list.marked.is_empty() || list.marked.iter().all(|(_, n)| n == "two"));
    }
}
