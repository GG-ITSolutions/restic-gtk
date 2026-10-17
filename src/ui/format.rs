//! User-facing formatting of sizes and times.

use gtk::glib;
use restic_gtk::backend::jiff::{Timestamp, ToSpan, Zoned, civil::Date, tz::TimeZone};

pub fn size(bytes: u64) -> String {
    glib::format_size(bytes).into()
}

fn local(time: &Zoned) -> Zoned {
    time.with_time_zone(TimeZone::system())
}

/// The date group a snapshot is listed under, newest groups first.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum DateGroup {
    Today,
    Yesterday,
    ThisWeek,
    /// Older: one group per month, ordered by how many months ago.
    Month {
        months_ago: i32,
        title: String,
    },
}

impl DateGroup {
    pub fn of(time: &Zoned, now: &Zoned) -> Self {
        let date = local(time).date();
        let today = local(now).date();
        let days_ago = (today - date).get_days();
        if days_ago <= 0 {
            Self::Today
        } else if days_ago == 1 {
            Self::Yesterday
        } else if days_ago < 7 {
            Self::ThisWeek
        } else {
            Self::Month {
                months_ago: months_between(date, today),
                title: date.strftime("%B %Y").to_string(),
            }
        }
    }

    pub fn title(&self) -> String {
        match self {
            Self::Today => "Today".into(),
            Self::Yesterday => "Yesterday".into(),
            Self::ThisWeek => "This Week".into(),
            Self::Month { title, .. } => title.clone(),
        }
    }
}

fn months_between(earlier: Date, later: Date) -> i32 {
    (i32::from(later.year()) - i32::from(earlier.year())) * 12 + i32::from(later.month())
        - i32::from(earlier.month())
}

/// A snapshot's time within its [`DateGroup`]: just as much as the group leaves open.
pub fn snapshot_time(time: &Zoned, group: &DateGroup) -> String {
    let time = local(time);
    match group {
        DateGroup::Today | DateGroup::Yesterday => time.strftime("%H:%M"),
        DateGroup::ThisWeek => time.strftime("%A, %H:%M"),
        DateGroup::Month { .. } => time.strftime("%-d %B, %H:%M"),
    }
    .to_string()
}

/// A full, unambiguous snapshot time, e.g. for page titles.
pub fn snapshot_time_full(time: &Zoned) -> String {
    local(time).strftime("%-d %B %Y, %H:%M").to_string()
}

/// Modification time of a file: short for the last half year, with year otherwise.
pub fn modified(time: Timestamp, now: Timestamp) -> String {
    let time = time.to_zoned(TimeZone::system());
    // Timestamps only do exact units, so half a year in hours.
    let recent = now
        .checked_sub((183 * 24).hours())
        .is_ok_and(|limit| time.timestamp() > limit);
    if recent {
        time.strftime("%-d %b, %H:%M")
    } else {
        time.strftime("%-d %b %Y")
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Zoned {
        s.parse().unwrap()
    }

    #[test]
    fn groups_by_distance_from_today() {
        let now = at("2026-03-10T12:00:00+01:00[+01:00]");
        let group = |s| DateGroup::of(&at(s), &now);
        assert_eq!(group("2026-03-10T08:00:00+01:00[+01:00]"), DateGroup::Today);
        assert_eq!(
            group("2026-03-09T23:59:00+01:00[+01:00]"),
            DateGroup::Yesterday
        );
        assert_eq!(
            group("2026-03-04T10:00:00+01:00[+01:00]"),
            DateGroup::ThisWeek
        );
        assert_eq!(
            group("2026-03-02T10:00:00+01:00[+01:00]"),
            DateGroup::Month {
                months_ago: 0,
                title: "March 2026".into()
            }
        );
        assert_eq!(
            group("2025-12-24T10:00:00+01:00[+01:00]"),
            DateGroup::Month {
                months_ago: 3,
                title: "December 2025".into()
            }
        );
    }

    #[test]
    fn groups_order_newest_first() {
        let month = |months_ago| DateGroup::Month {
            months_ago,
            title: String::new(),
        };
        let mut groups = vec![month(2), DateGroup::ThisWeek, month(0), DateGroup::Today];
        groups.sort();
        assert_eq!(
            groups,
            [DateGroup::Today, DateGroup::ThisWeek, month(0), month(2)]
        );
    }
}
