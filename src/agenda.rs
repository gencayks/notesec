//! The agenda: open tasks that have dates, grouped relative to today.
//!
//! Pure code (no GPUI): `app.rs` draws what `Agenda::build` returns.
//!
//! # Date format
//!
//! Logseq's: a line of the block (a continuation line under the task) that
//! starts with `SCHEDULED:` or `DEADLINE:` followed by a date in angle
//! brackets. In the file:
//!
//! ```text
//! - TODO write the report
//!   SCHEDULED: <2026-10-09 Fri>
//!   DEADLINE: <2026-10-12 Mon 17:00>
//! ```
//!
//! Inside the brackets the date (`YYYY-MM-DD`) comes first; then, in any
//! order, an optional weekday (letters only, not checked against the date,
//! as Logseq writes it in the user's language), an optional time `HH:MM`
//! and an optional Logseq repeater (`+1w`, `++1d`, `.+1m`), which is
//! ignored. Anything else inside the brackets, or a date that doesn't
//! exist, means no date. Both markers may share a line. The first valid one
//! of each kind wins.
//!
//! Open tasks are `TODO`, `DOING`, `LATER` and `NOW` (`DONE` is finished).
//! A task's date is the earlier of its scheduled and deadline dates. A task
//! on a journal page gets no date from the journal: only a marker counts,
//! as in Logseq.

use crate::display::DisplayBlock;
use crate::model::{task_split, Page, TaskState};
use chrono::{NaiveDate, NaiveTime};
use uuid::Uuid;

/// A date from a `SCHEDULED:` / `DEADLINE:` marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgendaDate {
    pub date: NaiveDate,
    /// `HH:MM`, if given.
    pub time: Option<NaiveTime>,
}

impl AgendaDate {
    /// `Fri 2026-10-09`, plus ` 17:00` with a time.
    pub fn label(&self) -> String {
        let day = day_label(self.date);
        match self.time {
            Some(time) => format!("{day} {}", time.format("%H:%M")),
            None => day,
        }
    }
}

/// `Fri 2026-10-09`: the day headers of the upcoming section.
pub fn day_label(date: NaiveDate) -> String {
    date.format("%a %Y-%m-%d").to_string()
}

/// The dates found in a block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dates {
    pub scheduled: Option<AgendaDate>,
    pub deadline: Option<AgendaDate>,
}

impl Dates {
    /// The date the task is filed under: the earlier of the two.
    pub fn first(&self) -> Option<NaiveDate> {
        match (self.scheduled, self.deadline) {
            (Some(s), Some(d)) => Some(s.date.min(d.date)),
            (s, d) => s.or(d).map(|x| x.date),
        }
    }
}

/// Find `SCHEDULED:` and `DEADLINE:` markers in block `content` (see the
/// module docs for the format). Markers count at the start of any line but
/// the first, which holds the task itself.
pub fn parse_dates(content: &str) -> Dates {
    let mut dates = Dates::default();
    for line in content.split('\n').skip(1) {
        let mut rest = line.trim_start();
        loop {
            let (slot, after) = if let Some(after) = rest.strip_prefix("SCHEDULED:") {
                (&mut dates.scheduled, after)
            } else if let Some(after) = rest.strip_prefix("DEADLINE:") {
                (&mut dates.deadline, after)
            } else {
                break;
            };
            let Some((date, after)) = parse_stamp(after.trim_start()) else {
                break;
            };
            slot.get_or_insert(date);
            rest = after.trim_start();
        }
    }
    dates
}

/// `<YYYY-MM-DD[ Ddd][ HH:MM][ repeater]>` at the start of `text`: the date
/// and what follows the `>`.
fn parse_stamp(text: &str) -> Option<(AgendaDate, &str)> {
    let inner = text.strip_prefix('<')?;
    let end = inner.find('>')?;
    let (inner, after) = (&inner[..end], &inner[end + 1..]);
    let mut parts = inner.split_whitespace();
    let date = NaiveDate::parse_from_str(parts.next()?, "%Y-%m-%d").ok()?;
    let mut time = None;
    for part in parts {
        if part.chars().all(char::is_alphabetic) {
            continue; // weekday
        }
        if is_repeater(part) {
            continue;
        }
        if time.is_none() && part.len() == 5 {
            if let Ok(t) = NaiveTime::parse_from_str(part, "%H:%M") {
                time = Some(t);
                continue;
            }
        }
        return None;
    }
    Some((AgendaDate { date, time }, after))
}

/// Logseq/org repeaters: `+1w`, `++2d`, `.+1m` (a number, then h/d/w/m/y).
fn is_repeater(part: &str) -> bool {
    let rest = part
        .strip_prefix(".+")
        .or_else(|| part.strip_prefix("++"))
        .or_else(|| part.strip_prefix('+'));
    let Some(rest) = rest else { return false };
    let Some(unit) = rest.chars().last() else {
        return false;
    };
    let number = &rest[..rest.len() - unit.len_utf8()];
    "hdwmy".contains(unit) && !number.is_empty() && number.chars().all(|c| c.is_ascii_digit())
}

/// One open task in the agenda.
#[derive(Clone, Debug, PartialEq)]
pub struct AgendaItem {
    /// Title of the page the block is on.
    pub page: String,
    /// The block, by id (indices shift when the page is edited).
    pub block: Uuid,
    pub state: TaskState,
    /// The task's first line as shown in the reading view (no type prefix,
    /// keyword or markdown markers).
    pub text: String,
    pub dates: Dates,
}

/// Open tasks, grouped relative to a day.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Agenda {
    /// Dated before today, oldest first.
    pub overdue: Vec<AgendaItem>,
    pub today: Vec<AgendaItem>,
    /// Dated after today, one entry per day, soonest first.
    pub upcoming: Vec<(NaiveDate, Vec<AgendaItem>)>,
    /// Open tasks without a date marker, last.
    pub unscheduled: Vec<AgendaItem>,
}

impl Agenda {
    /// Collect every open task in `pages` and group it relative to `today`.
    /// Within a group (or day), started tasks (`DOING`, `NOW`) come first,
    /// then by page title, then in page order; overdue tasks are sorted by
    /// date before that.
    pub fn build(pages: &[Page], today: NaiveDate) -> Agenda {
        let mut agenda = Agenda::default();
        let mut upcoming: Vec<AgendaItem> = Vec::new();
        for page in pages {
            for block in &page.blocks {
                let (_, state, _) = task_split(&block.content);
                let Some(state) = state.filter(|s| *s != TaskState::Done) else {
                    continue;
                };
                let first_line = block.content.split('\n').next().unwrap_or("");
                let item = AgendaItem {
                    page: page.title.clone(),
                    block: block.id,
                    state,
                    text: DisplayBlock::new(first_line).text,
                    dates: parse_dates(&block.content),
                };
                match item.dates.first() {
                    None => agenda.unscheduled.push(item),
                    Some(date) if date < today => agenda.overdue.push(item),
                    Some(date) if date == today => agenda.today.push(item),
                    Some(_) => upcoming.push(item),
                }
            }
        }
        // Stable sorts: equal keys keep page order.
        let key = |item: &AgendaItem| (!item.state.is_started(), item.page.to_lowercase());
        agenda.overdue.sort_by_key(|i| (i.dates.first(), key(i)));
        agenda.today.sort_by_key(key);
        agenda.unscheduled.sort_by_key(key);
        upcoming.sort_by_key(|i| (i.dates.first(), key(i)));
        for item in upcoming {
            let date = item.dates.first().unwrap_or(today);
            match agenda.upcoming.last_mut() {
                Some((day, items)) if *day == date => items.push(item),
                _ => agenda.upcoming.push((date, vec![item])),
            }
        }
        agenda
    }

    pub fn is_empty(&self) -> bool {
        self.overdue.is_empty()
            && self.today.is_empty()
            && self.upcoming.is_empty()
            && self.unscheduled.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn at(date: &str, time: Option<&str>) -> Option<AgendaDate> {
        Some(AgendaDate {
            date: day(date),
            time: time.map(|t| NaiveTime::parse_from_str(t, "%H:%M").unwrap()),
        })
    }

    #[test]
    fn parses_scheduled_and_deadline_with_weekday_time_and_repeater() {
        let d =
            parse_dates("TODO x\nSCHEDULED: <2026-10-09 Fri>\nDEADLINE: <2026-10-12 Mon 17:00>");
        assert_eq!(d.scheduled, at("2026-10-09", None));
        assert_eq!(d.deadline, at("2026-10-12", Some("17:00")));
        assert_eq!(d.first(), Some(day("2026-10-09")));

        // No weekday; a time without one; a repeater; a localised weekday.
        assert_eq!(
            parse_dates("TODO x\nSCHEDULED: <2026-10-09>").scheduled,
            at("2026-10-09", None)
        );
        assert_eq!(
            parse_dates("TODO x\n  DEADLINE: <2026-10-09 09:30>").deadline,
            at("2026-10-09", Some("09:30"))
        );
        assert_eq!(
            parse_dates("TODO x\nSCHEDULED: <2026-10-09 Fri .+1w>").scheduled,
            at("2026-10-09", None)
        );
        assert_eq!(
            parse_dates("TODO x\nSCHEDULED: <2026-10-09 Fr.>").scheduled,
            None,
            "only letters count as a weekday"
        );
        assert_eq!(
            parse_dates("TODO x\nSCHEDULED: <2026-10-09 Fre>").scheduled,
            at("2026-10-09", None)
        );
        // Both on one line, the earlier one wins for grouping.
        let d = parse_dates("TODO x\nDEADLINE: <2026-10-05> SCHEDULED: <2026-10-07 Wed>");
        assert_eq!(d.deadline, at("2026-10-05", None));
        assert_eq!(d.scheduled, at("2026-10-07", None));
        assert_eq!(d.first(), Some(day("2026-10-05")));
        // The first valid marker of a kind wins.
        assert_eq!(
            parse_dates("TODO x\nSCHEDULED: <2026-10-01>\nSCHEDULED: <2026-10-02>").scheduled,
            at("2026-10-01", None)
        );
    }

    #[test]
    fn rejects_malformed_markers() {
        for content in [
            "TODO x",
            // On the task's own line: not a marker line.
            "SCHEDULED: <2026-10-09>",
            "TODO x\nSCHEDULED: 2026-10-09",
            "TODO x\nSCHEDULED: <2026-10-09",
            "TODO x\nSCHEDULED: <2026-02-30>",
            "TODO x\nSCHEDULED: <09.10.2026>",
            "TODO x\nSCHEDULED: <2026-10-09 25:00>",
            "TODO x\nSCHEDULED: <2026-10-09 Fri lunch!>",
            "TODO x\nscheduled: <2026-10-09>",
            "TODO x\nsee SCHEDULED: <2026-10-09>",
        ] {
            assert_eq!(parse_dates(content), Dates::default(), "{content}");
        }
    }

    fn page(title: &str, journal: bool, md: &str) -> Page {
        Page::from_markdown(title, journal, md)
    }

    fn texts(items: &[AgendaItem]) -> Vec<&str> {
        items.iter().map(|i| i.text.as_str()).collect()
    }

    #[test]
    fn groups_open_tasks_relative_to_today() {
        let today = day("2026-10-08");
        let pages = vec![
            page(
                "Work",
                false,
                "- TODO late report\n  DEADLINE: <2026-10-01 Thu>\n\
                 - TODO **today** in [[Work]]\n  SCHEDULED: <2026-10-08 Thu>\n\
                 - DONE finished\n  SCHEDULED: <2026-10-08 Thu>\n\
                 - TODO next week\n  SCHEDULED: <2026-10-15 Thu>\n\
                 - TODO tomorrow b\n  SCHEDULED: <2026-10-09 Fri>\n\
                 - plain note\n  SCHEDULED: <2026-10-08 Thu>\n\
                 - TODO someday\n",
            ),
            page(
                "Alpha",
                false,
                "- DOING tomorrow a\n  SCHEDULED: <2026-10-09>\n\
                 - TODO earlier deadline wins\n  SCHEDULED: <2026-10-20>\n  DEADLINE: <2026-10-03>\n\
                 - ## LATER heading task\n  SCHEDULED: <2026-10-08>\n",
            ),
            // A task on a journal page gets no date from the journal.
            page("2026-10-08", true, "- TODO in a journal\n"),
        ];
        let agenda = Agenda::build(&pages, today);
        assert_eq!(
            texts(&agenda.overdue),
            vec!["late report", "earlier deadline wins"]
        );
        // Shown as in the reading view (emphasis markers hidden, links
        // kept); DONE and non-tasks left out. Same state: page title order.
        assert_eq!(
            texts(&agenda.today),
            vec!["heading task", "today in [[Work]]"]
        );
        let upcoming: Vec<(String, Vec<&str>)> = agenda
            .upcoming
            .iter()
            .map(|(d, items)| (day_label(*d), texts(items)))
            .collect();
        assert_eq!(
            upcoming,
            vec![
                // DOING first within a day.
                (
                    "Fri 2026-10-09".to_string(),
                    vec!["tomorrow a", "tomorrow b"]
                ),
                ("Thu 2026-10-15".to_string(), vec!["next week"]),
            ]
        );
        // By page title: "2026-10-08" sorts before "Work".
        assert_eq!(texts(&agenda.unscheduled), vec!["in a journal", "someday"]);
        assert_eq!(agenda.today[0].state, TaskState::Later);
        assert_eq!(agenda.today[1].page, "Work");
        assert!(!agenda.is_empty());
        assert!(Agenda::build(&[], today).is_empty());
    }

    #[test]
    fn date_labels() {
        assert_eq!(at("2026-10-09", None).unwrap().label(), "Fri 2026-10-09");
        assert_eq!(
            at("2026-10-12", Some("17:00")).unwrap().label(),
            "Mon 2026-10-12 17:00"
        );
    }
}
