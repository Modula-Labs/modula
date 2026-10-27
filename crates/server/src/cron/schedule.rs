//! Cron expressions: 5 fields (minute hour day-of-month month day-of-week),
//! always in UTC, plus the `@daily`-style nicknames. Parsed with `croner`;
//! described for humans here.

use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
use time::OffsetDateTime;

/// Longest expression accepted (they are short; this only bounds input).
const MAX_LEN: usize = 200;

pub fn parse(expr: &str) -> Result<Cron, String> {
    let expr = expr.trim();
    if expr.is_empty() || expr.len() > MAX_LEN || expr.contains('\0') {
        return Err("cron schedule must be a 5-field cron expression".into());
    }
    let nickname = expr.starts_with('@');
    if !nickname && expr.split_whitespace().count() != 5 {
        return Err(
            "cron schedule must have 5 fields: minute hour day-of-month month day-of-week".into(),
        );
    }
    CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .build()
        .parse(expr)
        .map_err(|e| format!("invalid cron schedule: {e}"))
}

/// The first due time strictly after `after` (whole seconds).
pub fn next_after(cron: &Cron, after: OffsetDateTime) -> Option<OffsetDateTime> {
    let from = jiff::Timestamp::from_second(after.unix_timestamp())
        .ok()?
        .to_zoned(jiff::tz::TimeZone::UTC);
    let next = cron.find_next_occurrence(&from, false).ok()?;
    OffsetDateTime::from_unix_timestamp(next.timestamp().as_second()).ok()
}

/// The next `n` due times after `after`.
pub fn upcoming(cron: &Cron, after: OffsetDateTime, n: usize) -> Vec<OffsetDateTime> {
    let mut out = Vec::with_capacity(n);
    let mut t = after;
    while out.len() < n {
        match next_after(cron, t) {
            Some(next) => {
                out.push(next);
                t = next;
            }
            None => break,
        }
    }
    out
}

const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

fn expand_nickname(expr: &str) -> &str {
    match expr.to_ascii_lowercase().as_str() {
        "@yearly" | "@annually" => "0 0 1 1 *",
        "@monthly" => "0 0 1 * *",
        "@weekly" => "0 0 * * 0",
        "@daily" | "@midnight" => "0 0 * * *",
        "@hourly" => "0 * * * *",
        _ => expr,
    }
}

fn num(f: &str, max: u32) -> Option<u32> {
    f.parse::<u32>().ok().filter(|n| *n <= max)
}

fn weekday(f: &str) -> Option<usize> {
    let names = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
    let lower = f.to_ascii_lowercase();
    names
        .iter()
        .position(|n| *n == lower)
        .or_else(|| num(f, 7).map(|n| n as usize % 7))
}

/// `a`, `a and b`, `a, b and c`.
fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// "every day at 03:00 UTC" and the like, for the common shapes; croner's
/// own description for the rest.
pub fn describe(expr: &str) -> String {
    let fallback = || match parse(expr) {
        Ok(c) => {
            let d = c.describe();
            let d = d.trim_end_matches('.');
            let mut chars = d.chars();
            match chars.next() {
                Some(f) => format!("{}{} (UTC)", f.to_lowercase(), chars.as_str()),
                None => expr.to_owned(),
            }
        }
        Err(e) => e,
    };
    let expanded = expand_nickname(expr.trim());
    let f: Vec<&str> = expanded.split_whitespace().collect();
    let [min, hour, dom, mon, dow] = f[..] else {
        return fallback();
    };
    let star = |s: &str| s == "*";
    let step = |s: &str| s.strip_prefix("*/").and_then(|n| n.parse::<u32>().ok());
    let rest_any = star(dom) && star(mon) && star(dow);
    if star(min) && star(hour) && rest_any {
        return "every minute".into();
    }
    if let (Some(n), true, true) = (step(min), star(hour), rest_any) {
        return match n {
            1 => "every minute".into(),
            n => format!("every {n} minutes"),
        };
    }
    let Some(m) = num(min, 59) else {
        return fallback();
    };
    if star(hour) && rest_any {
        return match m {
            0 => "every hour, on the hour".into(),
            m => format!("every hour at {m} minutes past"),
        };
    }
    if let (Some(n), true) = (step(hour), rest_any) {
        return format!("every {n} hours at :{m:02} UTC");
    }
    let hours: Option<Vec<u32>> = hour.split(',').map(|h| num(h, 23)).collect();
    let Some(hours) = hours else {
        return fallback();
    };
    let at = format!(
        "at {} UTC",
        and_list(
            &hours
                .iter()
                .map(|h| format!("{h:02}:{m:02}"))
                .collect::<Vec<_>>()
        )
    );
    match (star(dom), star(mon), star(dow)) {
        (true, true, true) => format!("every day {at}"),
        (true, true, false) => {
            if dow == "1-5" || dow.eq_ignore_ascii_case("mon-fri") {
                return format!("every weekday {at}");
            }
            let days: Option<Vec<String>> = dow
                .split(',')
                .map(|d| weekday(d).map(|i| DAYS[i].to_owned()))
                .collect();
            match days {
                Some(days) => format!("every {} {at}", and_list(&days)),
                None => fallback(),
            }
        }
        (false, true, true) => match num(dom, 31).filter(|d| *d >= 1) {
            Some(d) => format!("on day {d} of every month {at}"),
            None => fallback(),
        },
        (false, false, true) => match (
            num(dom, 31).filter(|d| *d >= 1),
            num(mon, 12).filter(|m| *m >= 1),
        ) {
            (Some(d), Some(mo)) => format!("every year on {} {d} {at}", MONTHS[mo as usize - 1]),
            _ => fallback(),
        },
        _ => fallback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn five_fields_and_nicknames_only() {
        for ok in [
            "0 3 * * *",
            "*/15 * * * *",
            "0 9 * * 1-5",
            "30 2 1 * *",
            "0 0 * * MON",
            "@daily",
            "@hourly",
        ] {
            assert!(parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "* * * *",
            "0 0 3 * * *",
            "0 0 3 * * * 2027",
            "61 * * * *",
            "0 24 * * *",
            "0 0 32 * *",
            "every day",
            "@sometimes",
            "0 3 * * *\0",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn next_times_are_utc_and_strictly_after() {
        let c = parse("0 3 * * *").unwrap();
        let now = datetime!(2026-10-24 02:59:59 UTC);
        assert_eq!(next_after(&c, now), Some(datetime!(2026-10-24 03:00 UTC)));
        // Exactly at a due time: the next one.
        let at = datetime!(2026-10-24 03:00 UTC);
        assert_eq!(next_after(&c, at), Some(datetime!(2026-10-25 03:00 UTC)));
        assert_eq!(
            upcoming(&parse("*/30 * * * *").unwrap(), at, 3),
            [
                datetime!(2026-10-24 03:30 UTC),
                datetime!(2026-10-24 04:00 UTC),
                datetime!(2026-10-24 04:30 UTC)
            ]
        );
    }

    #[test]
    fn descriptions() {
        for (expr, want) in [
            ("* * * * *", "every minute"),
            ("*/5 * * * *", "every 5 minutes"),
            ("0 * * * *", "every hour, on the hour"),
            ("15 * * * *", "every hour at 15 minutes past"),
            ("0 */6 * * *", "every 6 hours at :00 UTC"),
            ("0 3 * * *", "every day at 03:00 UTC"),
            ("@daily", "every day at 00:00 UTC"),
            ("30 8,20 * * *", "every day at 08:30 and 20:30 UTC"),
            ("0 9 * * 1-5", "every weekday at 09:00 UTC"),
            ("0 9 * * MON", "every Monday at 09:00 UTC"),
            (
                "0 9 * * 1,3,5",
                "every Monday, Wednesday and Friday at 09:00 UTC",
            ),
            ("0 0 * * 0", "every Sunday at 00:00 UTC"),
            ("30 2 1 * *", "on day 1 of every month at 02:30 UTC"),
            ("0 0 1 1 *", "every year on January 1 at 00:00 UTC"),
        ] {
            assert_eq!(describe(expr), want, "{expr}");
        }
        // Anything else: croner's wording, marked UTC.
        let other = describe("0 0-6 * * *");
        assert!(other.ends_with("(UTC)"), "{other}");
        assert!(other.starts_with("at minute 0"), "{other}");
    }
}
