//! Authenticity of release metadata: signed manifests and their expiry.
//!
//! Signing each artifact proves *what* a file is, but not that the manifest
//! listing it is current. Whoever controls the host (or the network, without
//! TLS pinning) could serve an old, validly signed manifest forever, so users
//! never learn about a security release — a *freeze* attack. A signed manifest
//! with an `expires` time bounds how long such a replay can work.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::http::{self, Metadata};
use crate::verify;

/// Fetch the text at `url` and verify it against the minisign signature at
/// `signature_url`, made by any of `keys`. Returns the verified text.
pub(super) fn fetch_signed(url: &str, signature_url: &str, keys: &[String]) -> Result<String> {
    let body = http::get_string(url, &[], Metadata::Document)?;
    let signature = http::get_string(signature_url, &[], Metadata::Sidecar)?;
    let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
    verify::verify_minisign_bytes(body.as_bytes(), &keys, &signature)?;
    Ok(body)
}

/// Fail when `expires`, an RFC 3339 time such as `2026-11-01T00:00:00Z`, is
/// not after `now`. An unparseable value fails closed.
pub(super) fn check_expiry(expires: &str, now: SystemTime) -> Result<()> {
    let deadline = unix_seconds(expires)
        .ok_or_else(|| Error::Parse(format!("`expires` is not an RFC 3339 time: {expires}")))?;
    let now = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    if now < deadline {
        Ok(())
    } else {
        Err(Error::ManifestExpired {
            expires: expires.to_string(),
        })
    }
}

/// Seconds since the Unix epoch for an RFC 3339 date-time. Fractional seconds
/// are dropped.
pub(super) fn unix_seconds(text: &str) -> Option<i64> {
    let (date, rest) = text.trim().split_once(['T', 't', ' '])?;
    let (clock, offset) = split_offset(rest)?;
    let [year, month, day] = fields(date, '-')?;
    let [hour, minute, second] = fields(clock.split('.').next()?, ':')?;
    let valid = (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && hour < 24
        && minute < 60
        && second <= 60;
    valid.then(|| {
        days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second - offset
    })
}

/// Split `12:00:00+08:00` into the clock and the offset in seconds east of UTC.
fn split_offset(rest: &str) -> Option<(&str, i64)> {
    if let Some(clock) = rest.strip_suffix(['Z', 'z']) {
        return Some((clock, 0));
    }
    let at = rest.rfind(['+', '-'])?;
    let (clock, offset) = rest.split_at(at);
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let (hours, minutes) = offset.get(1..)?.split_once(':')?;
    let seconds = hours.parse::<i64>().ok()? * 3_600 + minutes.parse::<i64>().ok()? * 60;
    Some((clock, sign * seconds))
}

/// Three numeric fields separated by `sep`, such as `2026-10-11`.
fn fields(text: &str, sep: char) -> Option<[i64; 3]> {
    let mut parts = text.split(sep).map(|p| p.parse::<i64>().ok());
    let fields = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(fields)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
pub(super) fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parses_rfc3339_times() {
        assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_seconds("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(
            unix_seconds("2026-10-11T12:34:56.789Z"),
            Some(1_791_722_096)
        );
        assert_eq!(
            unix_seconds("2026-10-11T20:34:56+08:00"),
            unix_seconds("2026-10-11T12:34:56Z")
        );
        assert_eq!(
            unix_seconds("2026-10-11T07:34:56-05:00"),
            unix_seconds("2026-10-11T12:34:56Z")
        );
        for bad in [
            "2026-10-11",
            "2026-13-01T00:00:00Z",
            "2026-10-11T25:00:00Z",
            "soon",
        ] {
            assert_eq!(unix_seconds(bad), None, "{bad}");
        }
    }

    #[test]
    fn rejects_an_expired_manifest() {
        let at = |s: u64| UNIX_EPOCH + Duration::from_secs(s);
        let expires = "2026-10-11T12:34:56Z";
        assert!(check_expiry(expires, at(1_791_722_095)).is_ok());
        assert!(matches!(
            check_expiry(expires, at(1_791_722_096)),
            Err(Error::ManifestExpired { .. })
        ));
        assert!(matches!(
            check_expiry("next week", at(0)),
            Err(Error::Parse(_))
        ));
    }
}
