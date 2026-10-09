//! Month calendar (roadmap v0.3.0 feature 5, docs/CALENDAR.md): a tab
//! with a month grid opened from the sidebar or the palette. Days whose
//! journal has entries are highlighted; clicking a day opens that journal,
//! creating it first. Journals are date-named pages already, so the vault
//! format doesn't change. Weeks start on Monday. Only `div`/`on_click`
//! shapes the grid — the same GPUI pieces every other view uses.

use super::*;
use chrono::{Datelike, NaiveDate};

/// The month on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) struct CalendarMonth {
    pub(in crate::app) year: i32,
    pub(in crate::app) month: u32,
}

impl CalendarMonth {
    /// This month, in local time.
    pub(in crate::app) fn current() -> Self {
        let today = chrono::Local::now().date_naive();
        CalendarMonth {
            year: today.year(),
            month: today.month(),
        }
    }

    pub(in crate::app) fn prev(self) -> Self {
        if self.month == 1 {
            CalendarMonth {
                year: self.year - 1,
                month: 12,
            }
        } else {
            CalendarMonth {
                year: self.year,
                month: self.month - 1,
            }
        }
    }

    pub(in crate::app) fn next(self) -> Self {
        if self.month == 12 {
            CalendarMonth {
                year: self.year + 1,
                month: 1,
            }
        } else {
            CalendarMonth {
                year: self.year,
                month: self.month + 1,
            }
        }
    }

    /// Leading blank cells (weeks start Monday) and the month's days.
    pub(in crate::app) fn grid(self) -> (u32, Vec<u32>) {
        let Some(first) = NaiveDate::from_ymd_opt(self.year, self.month, 1) else {
            return (0, Vec::new());
        };
        let blanks = first.weekday().num_days_from_monday();
        let days: Vec<u32> = (1..=31)
            .filter_map(|d| NaiveDate::from_ymd_opt(self.year, self.month, d))
            .map(|date| date.day())
            .collect();
        (blanks, days)
    }

    /// "October 2026".
    pub(in crate::app) fn title(self) -> String {
        NaiveDate::from_ymd_opt(self.year, self.month, 1)
            .map(|first| first.format("%B %Y").to_string())
            .unwrap_or_default()
    }

    /// The journal title for one of its days ("2026-10-09").
    pub(in crate::app) fn date(self, day: u32) -> String {
        format!("{:04}-{:02}-{day:02}", self.year, self.month)
    }
}

impl NoteSec {
    /// "Calendar" (palette, sidebar): open or focus the calendar tab.
    pub(in crate::app) fn show_calendar(&mut self, cx: &mut Context<Self>) {
        self.stop_edit(cx);
        self.leave_right_pane();
        self.tabs.open(TabTarget::Calendar);
        self.apply_tab(cx);
    }

    pub(in crate::app) fn on_open_calendar(
        &mut self,
        _: &OpenCalendar,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_calendar(cx);
    }

    /// Titles of journals with entries (at least one non-blank block).
    /// An empty journal (created, never written) doesn't highlight.
    fn journals_with_entries(&self) -> std::collections::HashSet<String> {
        self.pages
            .iter()
            .filter(|p| p.is_journal && p.blocks.iter().any(|b| !b.content.trim().is_empty()))
            .map(|p| p.title.clone())
            .collect()
    }

    /// Click on a day: open that journal in a tab (the calendar stays),
    /// creating it first like today's journal.
    fn open_calendar_day(&mut self, title: &str, cx: &mut Context<Self>) {
        self.stop_edit(cx);
        if self.find_journal(title).is_none() {
            let page = create_journal(&self.storage, title);
            self.add_page(page);
        }
        self.navigate(title, Nav::Tab, cx);
    }

    /// The month grid: weekday header, one row per week, highlighted days
    /// for journals with entries, `<` / `>` to move and `Today` to jump
    /// back.
    pub(in crate::app) fn render_calendar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let shown = self.calendar;
        let (blanks, days) = shown.grid();
        let entries = self.journals_with_entries();
        let today = chrono::Local::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        let small = |id: &'static str, label: &str| {
            div()
                .id(id)
                .debug_selector(move || id.to_string())
                .px_2()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .text_color(theme.muted)
                .cursor_pointer()
                .hover(|d| d.bg(theme.selected_bg))
                .child(label.to_string())
        };

        let mut cells: Vec<AnyElement> = Vec::new();
        for _ in 0..blanks {
            cells.push(div().flex_1().min_w_0().into_any_element());
        }
        for day in days {
            let title = shown.date(day);
            let marked = entries.contains(&title);
            let is_today = title == today;
            cells.push(
                div()
                    .id(SharedString::from(format!("calendar-day-{title}")))
                    .debug_selector({
                        let selector = format!("calendar-day-{title}");
                        move || selector.clone()
                    })
                    .flex_1()
                    .min_w_0()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(if marked { theme.accent } else { theme.border })
                    .when(marked, |d| d.bg(theme.selected_bg))
                    .cursor_pointer()
                    .hover(|d| d.bg(theme.selected_bg))
                    .child(
                        div()
                            .font_weight(if is_today || marked {
                                FontWeight::BOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(if marked { theme.accent } else { theme.text })
                            .child(day.to_string()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_calendar_day(&title, cx);
                    }))
                    .into_any_element(),
            );
        }
        while cells.len() % 7 != 0 {
            cells.push(div().flex_1().min_w_0().into_any_element());
        }
        let mut weeks: Vec<Vec<AnyElement>> = vec![Vec::new()];
        for cell in cells {
            if weeks.last().map_or(false, |week| week.len() == 7) {
                weeks.push(Vec::new());
            }
            weeks.last_mut().unwrap().push(cell);
        }
        let weeks: Vec<AnyElement> = weeks
            .into_iter()
            .map(|week| {
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .children(week)
                    .into_any_element()
            })
            .collect();

        div()
            .id("main")
            .flex_1()
            .h_full()
            .overflow_y_scroll()
            .p_8()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(self.ui_size() * 1.9))
                            .text_color(theme.text)
                            .child(shown.title()),
                    )
                    .child(
                        small("calendar-prev", "<").on_click(cx.listener(|this, _, _, cx| {
                            this.calendar = this.calendar.prev();
                            cx.notify();
                        })),
                    )
                    .child(small("calendar-today", "Today").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.calendar = CalendarMonth::current();
                            cx.notify();
                        },
                    )))
                    .child(
                        small("calendar-next", ">").on_click(cx.listener(|this, _, _, cx| {
                            this.calendar = this.calendar.next();
                            cx.notify();
                        })),
                    ),
            )
            .child(
                div()
                    .debug_selector(|| "calendar-title".to_string())
                    .text_color(theme.muted)
                    .child("Days with journal entries are highlighted; click a day to open it."),
            )
            .child(div().flex().flex_row().gap_1().children(
                ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].map(|name| {
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(theme.muted)
                        .child(name.to_string())
                        .into_any_element()
                }),
            ))
            .children(weeks)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_starts_monday_and_counts_days() {
        // 2026-10-01 is a Thursday: 3 blanks, then 31 days.
        assert_eq!(
            CalendarMonth {
                year: 2026,
                month: 10
            }
            .grid(),
            (3, (1..=31).collect::<Vec<_>>(),)
        );
        // February 2026: 28 days, starting on a Sunday (6 blanks).
        let (blanks, days) = CalendarMonth {
            year: 2026,
            month: 2,
        }
        .grid();
        assert_eq!((blanks, days.len()), (6, 28));
        // A Monday the 1st needs no blanks (June 2026).
        assert_eq!(
            CalendarMonth {
                year: 2026,
                month: 6
            }
            .grid()
            .0,
            0
        );
    }

    #[test]
    fn months_turn_over_the_year() {
        let dec = CalendarMonth {
            year: 2026,
            month: 12,
        };
        assert_eq!(
            dec.next(),
            CalendarMonth {
                year: 2027,
                month: 1
            }
        );
        assert_eq!(
            (dec.next().prev(), dec.prev()),
            (
                dec,
                CalendarMonth {
                    year: 2026,
                    month: 11
                }
            )
        );
        assert_eq!(
            CalendarMonth {
                year: 2027,
                month: 1
            }
            .prev(),
            CalendarMonth {
                year: 2026,
                month: 12
            }
        );
    }

    #[test]
    fn titles_and_dates_read_like_journals() {
        let shown = CalendarMonth {
            year: 2026,
            month: 10,
        };
        assert_eq!(shown.title(), "October 2026");
        assert_eq!(shown.date(9), "2026-10-09");
        assert_eq!(shown.date(31), "2026-10-31");
    }
}

#[cfg(test)]
mod ui_tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, VisualTestContext};

    /// A window on a graph with regular `pages` and `journals`
    /// (`title` is `YYYY-MM-DD`), the first page shown.
    fn setup<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
        pages: &[(&str, &str)],
        journals: &[(&str, &str)],
    ) -> (
        Entity<NoteSec>,
        &'a mut VisualTestContext,
        std::path::PathBuf,
    ) {
        let dir =
            std::env::temp_dir().join(format!("notesec-calendar-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Storage::open(dir.clone()).unwrap();
        for (title, markdown) in pages {
            std::fs::write(dir.join(format!("pages/{title}.md")), markdown).unwrap();
        }
        for (title, markdown) in journals {
            let file = dir.join(format!("journals/{}.md", title.replace('-', "_")));
            std::fs::write(file, markdown).unwrap();
        }
        cx.update(bind_keys);
        let (view, cx) =
            cx.add_window_view(|window, cx| NoteSec::new(storage, Config::default(), window, cx));
        let first = pages[0].0.to_string();
        view.update(cx, |app, cx| {
            app.selected = app.find_page(&first).unwrap();
            app.tabs = Tabs::new(TabTarget::Page(first.clone()));
            // October 2026, so fixed journal dates are on screen.
            app.calendar = CalendarMonth {
                year: 2026,
                month: 10,
            };
            cx.notify();
        });
        cx.run_until_parked();
        (view, cx, dir)
    }

    fn has(cx: &mut VisualTestContext, selector: &str) -> bool {
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        cx.debug_bounds(selector).is_some()
    }

    fn click_on(cx: &mut VisualTestContext, selector: &str) {
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        let bounds = cx.debug_bounds(selector).expect(selector);
        cx.simulate_click(bounds.center(), Modifiers::none());
    }

    const PAGES: &[(&str, &str)] = &[("Home", "- hello\n")];
    const JOURNALS: &[(&str, &str)] = &[("2026-10-09", "- wrote tests\n"), ("2026-10-10", "   \n")];

    #[gpui::test]
    fn sidebar_and_palette_open_the_month(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "open", PAGES, JOURNALS);
        click_on(cx, "sidebar-calendar");
        assert!(view.update(cx, |app, _| app.mode == Mode::Calendar));
        assert!(has(cx, "calendar-title") && has(cx, "calendar-day-2026-10-09"));
        // Back to a page, then the palette entry reopens it.
        click_on(cx, "tab-0");
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("calendar");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(view.update(cx, |app, _| app.mode == Mode::Calendar));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn only_journals_with_entries_count(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "entries", PAGES, JOURNALS);
        view.update(cx, |app, _| {
            let entries = app.journals_with_entries();
            assert!(entries.contains("2026-10-09"));
            assert!(!entries.contains("2026-10-10"), "blank journal: no entries");
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn clicking_a_day_opens_its_journal(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "open-day", PAGES, JOURNALS);
        click_on(cx, "sidebar-calendar");
        click_on(cx, "calendar-day-2026-10-09");
        view.update(cx, |app, _| {
            assert_eq!(app.mode, Mode::Notes);
            assert_eq!(app.pages[app.selected].title, "2026-10-09");
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn clicking_an_empty_day_creates_the_journal(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "create-day", PAGES, JOURNALS);
        click_on(cx, "sidebar-calendar");
        click_on(cx, "calendar-day-2026-10-11");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "2026-10-11");
        });
        assert!(dir.join("journals/2026_10_11.md").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn month_buttons_move_and_today_jumps_back(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "nav", PAGES, JOURNALS);
        click_on(cx, "sidebar-calendar");
        click_on(cx, "calendar-next");
        assert!(view.update(cx, |app, _| app.calendar
            == CalendarMonth {
                year: 2026,
                month: 11
            }));
        assert!(has(cx, "calendar-day-2026-11-30"));
        click_on(cx, "calendar-prev");
        click_on(cx, "calendar-prev");
        assert!(view.update(cx, |app, _| app.calendar
            == CalendarMonth {
                year: 2026,
                month: 9
            }));
        click_on(cx, "calendar-today");
        assert!(view.update(cx, |app, _| app.calendar == CalendarMonth::current()));
        let _ = std::fs::remove_dir_all(dir);
    }
}
