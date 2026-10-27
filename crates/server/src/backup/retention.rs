//! Grandfather-father-son retention. A backup is kept while it is the newest
//! backup of its UTC day (for `daily_days`), of its ISO week (for
//! `weekly_days`) or of its month (for `monthly_days`). The newest backup is
//! always kept, so a long outage never prunes the last good copy.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Policy {
    pub daily_days: i32,
    pub weekly_days: i32,
    pub monthly_days: i32,
}

/// Roughly 7 dailies, 4 weeklies and 3 monthlies.
pub const DEFAULT: Policy = Policy {
    daily_days: 6,
    weekly_days: 27,
    monthly_days: 89,
};

/// The ids of `backups` (id, finished at) that no rule keeps.
pub fn expired<T: Copy>(
    backups: &[(T, OffsetDateTime)],
    now: OffsetDateTime,
    policy: &Policy,
) -> Vec<T> {
    let mut newest_first = backups.to_vec();
    newest_first.sort_by_key(|(_, at)| std::cmp::Reverse(*at));
    let (mut days, mut weeks, mut months) = (HashSet::new(), HashSet::new(), HashSet::new());
    let within = |at: OffsetDateTime, days: i32| now - at < Duration::days(i64::from(days));
    let mut out = Vec::new();
    for (i, (id, at)) in newest_first.into_iter().enumerate() {
        let utc = at.to_offset(time::UtcOffset::UTC);
        let (year, week, _) = utc.to_iso_week_date();
        // Evaluate every rule: each one claims its bucket's newest backup.
        let daily = within(at, policy.daily_days) && days.insert(utc.date());
        let weekly = within(at, policy.weekly_days) && weeks.insert((year, week));
        let monthly = within(at, policy.monthly_days) && months.insert((utc.year(), utc.month()));
        if !(i == 0 || daily || weekly || monthly) {
            out.push(id);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    /// One backup a day at 03:00 for 200 days.
    fn daily(until: OffsetDateTime, count: i64) -> Vec<(i64, OffsetDateTime)> {
        (0..count).map(|i| (i, until - Duration::days(i))).collect()
    }

    #[test]
    fn keeps_dailies_weeklies_and_monthlies() {
        let last = datetime!(2026-10-23 03:00 UTC); // a Friday
        let now = last + Duration::minutes(30);
        let backups = daily(last, 200);
        let expired: HashSet<i64> = expired(&backups, now, &DEFAULT).into_iter().collect();
        for (id, at) in &backups {
            let age = now - *at;
            let next = *at + Duration::days(1);
            let last_of_week = next.iso_week() != at.iso_week();
            let last_of_month = next.month() != at.month();
            let keep = age < Duration::days(6)
                || (age < Duration::days(27) && last_of_week)
                || (age < Duration::days(89) && last_of_month);
            assert_eq!(!expired.contains(id), keep, "{at} (age {age})");
        }
        let kept = backups.len() - expired.len();
        assert!((10..=14).contains(&kept), "kept {kept}");
    }

    #[test]
    fn the_newest_backup_always_survives() {
        let now = datetime!(2026-10-23 03:00 UTC);
        let old = daily(now - Duration::days(400), 3);
        let zero = Policy {
            daily_days: 0,
            weekly_days: 0,
            monthly_days: 0,
        };
        let mut gone = expired(&old, now, &zero);
        gone.sort();
        assert_eq!(gone, [1, 2]);
        assert!(expired::<i64>(&[], now, &DEFAULT).is_empty());
    }
}
