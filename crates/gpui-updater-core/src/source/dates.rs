//! Release publication times, in the date formats the sources publish.
//!
//! A publication time is informational: it is shown to the user, never used
//! for a security decision. A value that does not parse is dropped rather than
//! failing the check.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::trust::{days_from_civil, unix_seconds};

/// An RFC 3339 date-time such as `2026-10-11T12:34:56Z` (static manifests,
/// the GitHub API).
pub(super) fn rfc3339(text: &str) -> Option<SystemTime> {
    let parsed = unix_seconds(text).and_then(from_unix);
    if parsed.is_none() {
        tracing::debug!(text, "ignoring an unparseable RFC 3339 publication time");
    }
    parsed
}

/// An RFC 2822 date-time such as `Wed, 09 Jan 2006 19:20:11 +0000`, the
/// format of an RSS `<pubDate>` (Sparkle appcasts).
pub(super) fn rfc2822(text: &str) -> Option<SystemTime> {
    let parsed = rfc2822_seconds(text).and_then(from_unix);
    if parsed.is_none() {
        tracing::debug!(text, "ignoring an unparseable RFC 2822 publication time");
    }
    parsed
}

fn from_unix(seconds: i64) -> Option<SystemTime> {
    let seconds = u64::try_from(seconds).ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

/// Seconds since the Unix epoch for an RFC 2822 date-time. The day of the week
/// is optional and not checked; seconds are optional; two-digit years follow
/// RFC 2822's obsolete-syntax rule (`< 50` is 20xx, otherwise 19xx).
fn rfc2822_seconds(text: &str) -> Option<i64> {
    let text = text.trim();
    let text = match text.split_once(',') {
        Some((_weekday, rest)) => rest,
        None => text,
    };
    let mut parts = text.split_whitespace();
    let day: i64 = parts.next()?.parse().ok()?;
    let month = month_number(parts.next()?)?;
    let year = match parts.next()? {
        two if two.len() == 2 => {
            let year: i64 = two.parse().ok()?;
            if year < 50 { 2000 + year } else { 1900 + year }
        }
        four => four.parse().ok()?,
    };
    let mut clock = parts.next()?.split(':').map(|p| p.parse::<i64>().ok());
    let hour = clock.next()??;
    let minute = clock.next()??;
    let second = clock.next().unwrap_or(Some(0))?;
    if clock.next().is_some() {
        return None;
    }
    let offset = zone_offset(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    let valid = (1..=31).contains(&day) && hour < 24 && minute < 60 && second <= 60;
    valid.then(|| {
        days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second - offset
    })
}

fn month_number(name: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let name = name.get(..3)?.to_ascii_lowercase();
    let index = MONTHS.iter().position(|m| *m == name)?;
    i64::try_from(index).ok().map(|i| i + 1)
}

/// Seconds east of UTC for `+hhmm`, `-hhmm`, or a zone name RFC 2822 allows.
fn zone_offset(zone: &str) -> Option<i64> {
    let hours = |h: i64| Some(h * 3_600);
    match zone.to_ascii_uppercase().as_str() {
        "UT" | "UTC" | "GMT" | "Z" => hours(0),
        "EDT" => hours(-4),
        "EST" | "CDT" => hours(-5),
        "CST" | "MDT" => hours(-6),
        "MST" | "PDT" => hours(-7),
        "PST" => hours(-8),
        numeric => {
            let sign = match numeric.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let digits = numeric.get(1..)?;
            if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let h: i64 = digits[..2].parse().ok()?;
            let m: i64 = digits[2..].parse().ok()?;
            Some(sign * (h * 3_600 + m * 60))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn parses_rss_pub_dates() {
        // The RSS 2.0 specification's own example.
        assert_eq!(
            rfc2822("Sat, 07 Sep 2002 00:00:01 GMT"),
            rfc3339("2002-09-07T00:00:01Z")
        );
        assert_eq!(
            rfc2822("Wed, 09 Jan 2006 19:20:11 +0000"),
            Some(at(1_136_834_411))
        );
        // Offsets, named zones, no weekday, no seconds, and two-digit years.
        assert_eq!(
            rfc2822("Sun, 11 Oct 2026 20:34:56 +0800"),
            rfc3339("2026-10-11T12:34:56Z")
        );
        assert_eq!(
            rfc2822("11 Oct 2026 07:34:56 EST"),
            rfc3339("2026-10-11T12:34:56Z")
        );
        assert_eq!(rfc2822("1 Jan 70 00:01 UT"), Some(at(60)));
        assert_eq!(
            rfc2822("1 January 2001 00:00:00 +0000"),
            Some(at(978_307_200))
        );
    }

    #[test]
    fn drops_what_does_not_parse() {
        for bad in [
            "",
            "yesterday",
            "Wed, 32 Jan 2006 19:20:11 +0000",
            "Wed, 09 Foo 2006 19:20:11 +0000",
            "Wed, 09 Jan 2006 24:00:00 +0000",
            "Wed, 09 Jan 2006 19:20:11 +00",
            "Wed, 09 Jan 2006 19:20:11 +0000 extra",
            "Thu, 01 Jan 1969 00:00:00 +0000",
        ] {
            assert_eq!(rfc2822(bad), None, "{bad}");
        }
        assert_eq!(rfc3339("2026-10-11"), None);
        assert_eq!(rfc3339("1969-12-31T23:59:59Z"), None);
    }
}
