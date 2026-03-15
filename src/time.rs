//! Shared UTC timestamp formatting.
//!
//! Chrono-free ISO 8601 formatting from Unix epochs using Hinnant's
//! `civil_from_days` algorithm.

use std::time::SystemTime;

/// Current Unix epoch in seconds.
pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_secs()
}

/// Current time as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn now_iso8601() -> String {
    format_iso8601(now_epoch())
}

/// Format a Unix epoch as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format_iso8601(epoch: u64) -> String {
    let days_since_epoch = (epoch / 86400).cast_signed();
    let time_of_day = epoch % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;
    let (year, month, day) = civil_from_days(days_since_epoch);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Convert days since 1970-01-01 to (year, month, day).
///
/// Howard Hinnant's algorithm. See:
/// <https://howardhinnant.github.io/date_algorithms.html#civil_from_days>
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = u32::try_from(z.rem_euclid(146_097)).expect("day-of-era fits in u32");
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = i64::from(yoe) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_zero() {
        assert_eq!(format_iso8601(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn y2k() {
        assert_eq!(format_iso8601(946_684_800), "2000-01-01T00:00:00Z");
    }

    #[test]
    fn with_time() {
        assert_eq!(
            format_iso8601(1_708_473_600 + 14 * 3600 + 30 * 60 + 45),
            "2024-02-21T14:30:45Z"
        );
    }

    #[test]
    fn now_is_valid() {
        let ts = now_iso8601();
        assert!(ts.ends_with('Z'));
        assert!(ts.contains('T'));
        assert_eq!(ts.len(), 20);
    }

    #[test]
    fn lexicographic_ordering() {
        let t1 = "2025-01-15T10:00:00Z";
        let t2 = "2025-01-15T10:00:01Z";
        let t3 = "2025-01-16T00:00:00Z";
        assert!(t1 < t2);
        assert!(t2 < t3);
    }
}
