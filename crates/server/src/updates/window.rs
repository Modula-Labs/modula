//! The maintenance window automatic upgrades wait for: some weekdays, a start
//! time and a time zone (default Sunday 03:00, server local time). A window
//! lasts an hour; an upgrade that starts in it may run past its end.

use jiff::civil::Weekday;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp, ToSpan};

pub const LENGTH: SignedDuration = SignedDuration::from_hours(1);

/// Weekdays as a bitmask: bit 0 = Monday … bit 6 = Sunday.
pub const SUNDAY: u8 = 1 << 6;

#[derive(Debug, Clone)]
pub struct Window {
    pub days: u8,
    /// Minutes after midnight, local time.
    pub minute: u16,
    pub tz: TimeZone,
}

/// A configured time zone: an IANA name, or empty for the server's own.
/// Unknown names fall back to UTC (they are validated when saved).
pub fn timezone(name: &str) -> TimeZone {
    if name.is_empty() {
        return TimeZone::system();
    }
    TimeZone::get(name).unwrap_or(TimeZone::UTC)
}

pub fn valid_timezone(name: &str) -> bool {
    name.is_empty() || (name.len() <= 64 && TimeZone::get(name).is_ok())
}

/// The server's own time zone, for display ("UTC" when it has no name).
pub fn system_timezone_name() -> String {
    TimeZone::system()
        .iana_name()
        .map_or_else(|| "UTC".to_owned(), str::to_owned)
}

fn bit(day: Weekday) -> u8 {
    1 << day.to_monday_zero_offset()
}

impl Window {
    /// Where the window opens on the day `date`, if that weekday is selected.
    fn start_on(&self, date: jiff::civil::Date) -> Option<Timestamp> {
        if self.days & bit(date.weekday()) == 0 {
            return None;
        }
        let at = date.at((self.minute / 60) as i8, (self.minute % 60) as i8, 0, 0);
        // A start inside a DST gap moves forward, like a wall clock would.
        at.to_zoned(self.tz.clone()).ok().map(|z| z.timestamp())
    }

    /// Whether `now` falls in a window (one that opened in the last hour).
    pub fn contains(&self, now: Timestamp) -> bool {
        let today = now.to_zoned(self.tz.clone()).date();
        // A window opened late yesterday may still be open after midnight.
        [today.yesterday().ok(), Some(today)]
            .into_iter()
            .flatten()
            .filter_map(|d| self.start_on(d))
            .any(|start| start <= now && now < start + LENGTH)
    }

    /// When the next window opens after `now` (`None` if no day is selected).
    pub fn next_start(&self, now: Timestamp) -> Option<Timestamp> {
        let today = now.to_zoned(self.tz.clone()).date();
        (0..=7)
            .filter_map(|i| today.checked_add(i.days()).ok())
            .filter_map(|d| self.start_on(d))
            .find(|start| *start > now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn sunday_3am(tz: &str) -> Window {
        Window {
            days: SUNDAY,
            minute: 3 * 60,
            tz: TimeZone::get(tz).unwrap(),
        }
    }

    #[test]
    fn sunday_three_am_for_an_hour() {
        let w = sunday_3am("UTC");
        // 2026-10-25 is a Sunday.
        assert!(!w.contains(at("2026-10-25T02:59:59Z")));
        assert!(w.contains(at("2026-10-25T03:00:00Z")));
        assert!(w.contains(at("2026-10-25T03:59:59Z")));
        assert!(!w.contains(at("2026-10-25T04:00:00Z")));
        assert!(!w.contains(at("2026-10-24T03:30:00Z")), "a Saturday");
        assert_eq!(
            w.next_start(at("2026-10-23T12:00:00Z")),
            Some(at("2026-10-25T03:00:00Z"))
        );
        // From inside a window, the next one is a week later.
        assert_eq!(
            w.next_start(at("2026-10-25T03:10:00Z")),
            Some(at("2026-11-01T03:00:00Z"))
        );
    }

    #[test]
    fn local_time_follows_the_zone_and_dst() {
        let w = sunday_3am("Europe/Berlin");
        // 25 Oct 2026: Berlin leaves summer time at 03:00 CEST → 02:00 CET,
        // so 03:00 local happens once, at 02:00 UTC.
        assert!(w.contains(at("2026-10-25T02:30:00Z")));
        assert!(!w.contains(at("2026-10-25T01:30:00Z")));
        // A week later, CET: 03:00 local = 02:00 UTC as well; in summer, 01:00 UTC.
        assert_eq!(
            w.next_start(at("2026-10-26T00:00:00Z")),
            Some(at("2026-11-01T02:00:00Z"))
        );
        assert!(w.contains(at("2026-06-07T01:15:00Z")));
    }

    #[test]
    fn windows_cross_midnight_and_pick_weekdays() {
        let w = Window {
            days: bit(Weekday::Tuesday) | bit(Weekday::Thursday),
            minute: 23 * 60 + 30,
            tz: TimeZone::UTC,
        };
        // Tuesday 2026-10-20 23:30 → Wednesday 00:30.
        assert!(w.contains(at("2026-10-20T23:45:00Z")));
        assert!(w.contains(at("2026-10-21T00:15:00Z")));
        assert!(!w.contains(at("2026-10-21T23:45:00Z")), "Wednesday");
        assert_eq!(
            w.next_start(at("2026-10-21T00:00:00Z")),
            Some(at("2026-10-22T23:30:00Z"))
        );
        let never = Window { days: 0, ..w };
        assert!(!never.contains(at("2026-10-20T23:45:00Z")));
        assert_eq!(never.next_start(at("2026-10-20T00:00:00Z")), None);
    }

    #[test]
    fn time_zones() {
        assert!(valid_timezone(""));
        assert!(valid_timezone("America/New_York"));
        assert!(!valid_timezone("Mars/Olympus_Mons"));
        assert_eq!(timezone("Mars/Olympus_Mons"), TimeZone::UTC);
    }
}
