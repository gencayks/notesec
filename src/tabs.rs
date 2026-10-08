//! The open tabs: an ordered list of what each tab shows plus which one is
//! active. Pure data (no GPUI), so the open/close/cycle rules are unit-tested
//! here; `app.rs` turns the active tab into the page, graph, agenda or
//! trash on screen.
//!
//! Tabs refer to pages by title, not by index: `pages` is re-sorted whenever
//! a page is added, which shifts indices but never titles.

/// What a tab shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TabTarget {
    /// The page with this title.
    Page(String),
    /// The page graph.
    Graph,
    /// The agenda (open tasks by date).
    Agenda,
    /// The trash (deleted pages, with Restore and Delete forever).
    Trash,
}

#[derive(Clone, Debug, Default)]
pub struct Tabs {
    pub tabs: Vec<TabTarget>,
    /// Index of the active tab; `None` only when there are no tabs.
    pub active: Option<usize>,
}

impl Tabs {
    /// One tab, active.
    pub fn new(first: TabTarget) -> Self {
        Tabs {
            tabs: vec![first],
            active: Some(0),
        }
    }

    pub fn active_target(&self) -> Option<&TabTarget> {
        self.active.and_then(|i| self.tabs.get(i))
    }

    /// Focus a tab showing `target` (the active one first, if it does), or
    /// add a new tab at the end and focus it.
    pub fn open(&mut self, target: TabTarget) {
        if self.active_target() == Some(&target) {
            return;
        }
        match self.tabs.iter().position(|t| *t == target) {
            Some(i) => self.active = Some(i),
            None => {
                self.tabs.push(target);
                self.active = Some(self.tabs.len() - 1);
            }
        }
    }

    /// Show `target` in the active tab instead of what it showed (like
    /// following a link in a browser). With no tabs, opens the first one.
    pub fn replace_active(&mut self, target: TabTarget) {
        match self.active {
            Some(i) => self.tabs[i] = target,
            None => self.open(target),
        }
    }

    /// Focus tab `ix` (ignored if out of range).
    pub fn select(&mut self, ix: usize) {
        if ix < self.tabs.len() {
            self.active = Some(ix);
        }
    }

    /// Close tab `ix`. Closing the active tab focuses the tab that takes
    /// its place (the one to its right), else the one to its left; closing
    /// the last tab leaves none. Closing another tab keeps the active one.
    pub fn close(&mut self, ix: usize) {
        if ix >= self.tabs.len() {
            return;
        }
        self.tabs.remove(ix);
        self.active = match self.active {
            _ if self.tabs.is_empty() => None,
            Some(a) if a == ix => Some(ix.min(self.tabs.len() - 1)),
            Some(a) if a > ix => Some(a - 1),
            other => other,
        };
    }

    /// Ctrl+Tab (`forward`) / Ctrl+Shift+Tab: the next or previous tab,
    /// wrapping around.
    pub fn cycle(&mut self, forward: bool) {
        let n = self.tabs.len();
        if let Some(a) = self.active.filter(|_| n > 1) {
            self.active = Some(if forward {
                (a + 1) % n
            } else {
                (a + n - 1) % n
            });
        }
    }

    /// A page was renamed: tabs showing `old` now show `new`.
    pub fn rename(&mut self, old: &str, new: &str) {
        for tab in &mut self.tabs {
            if matches!(tab, TabTarget::Page(t) if t == old) {
                *tab = TabTarget::Page(new.to_string());
            }
        }
    }

    /// Close every tab for which `keep` is false (e.g. pages that no longer
    /// exist), with the same focus rule as [`Tabs::close`].
    pub fn retain(&mut self, keep: impl Fn(&TabTarget) -> bool) {
        for ix in (0..self.tabs.len()).rev() {
            if !keep(&self.tabs[ix]) {
                self.close(ix);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(title: &str) -> TabTarget {
        TabTarget::Page(title.to_string())
    }

    #[test]
    fn open_focuses_an_existing_tab_or_adds_one() {
        let mut tabs = Tabs::new(page("A"));
        tabs.open(page("B"));
        assert_eq!((tabs.tabs.len(), tabs.active), (2, Some(1)));
        tabs.open(page("A"));
        assert_eq!((tabs.tabs.len(), tabs.active), (2, Some(0)));
        tabs.open(TabTarget::Graph);
        assert_eq!(tabs.tabs, [page("A"), page("B"), TabTarget::Graph]);
        assert_eq!(tabs.active, Some(2));
        // Replacing changes only the active tab.
        tabs.select(0);
        tabs.replace_active(page("C"));
        assert_eq!(tabs.tabs, [page("C"), page("B"), TabTarget::Graph]);
        assert_eq!(tabs.active, Some(0));
    }

    #[test]
    fn closing_picks_the_neighbour_and_can_empty() {
        let mut tabs = Tabs::new(page("A"));
        tabs.open(page("B"));
        tabs.open(page("C"));
        // Closing the active middle tab focuses the one now in its place.
        tabs.select(1);
        tabs.close(1);
        assert_eq!(
            (tabs.tabs.clone(), tabs.active),
            (vec![page("A"), page("C")], Some(1))
        );
        // Closing the active last tab focuses its left neighbour.
        tabs.close(1);
        assert_eq!(tabs.active, Some(0));
        // Closing an inactive tab keeps the active one.
        tabs.open(page("D"));
        tabs.close(0);
        assert_eq!((tabs.tabs.clone(), tabs.active), (vec![page("D")], Some(0)));
        tabs.close(0);
        assert_eq!((tabs.tabs.len(), tabs.active), (0, None));
        tabs.cycle(true);
        assert_eq!(tabs.active, None);
        // From empty, replacing opens a first tab.
        tabs.replace_active(page("E"));
        assert_eq!((tabs.tabs.clone(), tabs.active), (vec![page("E")], Some(0)));
    }

    #[test]
    fn cycling_wraps_both_ways() {
        let mut tabs = Tabs::new(page("A"));
        tabs.cycle(true);
        assert_eq!(tabs.active, Some(0), "one tab: nothing to cycle");
        tabs.open(page("B"));
        tabs.open(TabTarget::Graph);
        let mut seen = Vec::new();
        for _ in 0..3 {
            tabs.cycle(true);
            seen.push(tabs.active.unwrap());
        }
        assert_eq!(seen, [0, 1, 2]);
        tabs.cycle(false);
        tabs.cycle(false);
        assert_eq!(tabs.active, Some(0));
        tabs.cycle(false);
        assert_eq!(tabs.active, Some(2));
    }

    #[test]
    fn rename_keeps_position_and_focus() {
        let mut tabs = Tabs::new(page("A"));
        tabs.open(page("B"));
        tabs.open(TabTarget::Graph);
        tabs.select(1);
        tabs.rename("B", "C");
        assert_eq!(tabs.tabs, [page("A"), page("C"), TabTarget::Graph]);
        assert_eq!(tabs.active, Some(1));
    }

    #[test]
    fn retain_drops_tabs_and_refocuses() {
        let mut tabs = Tabs::new(page("A"));
        tabs.open(page("Gone"));
        tabs.open(page("B"));
        tabs.select(1);
        tabs.retain(|t| *t != page("Gone"));
        assert_eq!(
            (tabs.tabs.clone(), tabs.active),
            (vec![page("A"), page("B")], Some(1))
        );
    }
}
