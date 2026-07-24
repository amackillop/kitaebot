//! Pure schedule math for duties, on Unix epoch seconds (UTC).
//!
//! No calendar library: `Daily` is a time-of-day offset into the UTC
//! day, consistent with the epoch arithmetic in `crate::time`.

/// When a duty recurs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Schedule {
    /// Every N seconds after the last run.
    Every(u64),
    /// Daily at N seconds after UTC midnight.
    Daily(u32),
}

impl Schedule {
    /// The epoch at which the duty is next due.
    ///
    /// Anacron semantics: with no recorded run the duty is due now
    /// (fresh install or lost state — one catch-up run, spec 24), and
    /// callers set `last_run = now` after running, so an arbitrarily
    /// overdue duty fires once rather than once per missed period.
    pub fn next_due(self, last_run: Option<u64>, now: u64) -> u64 {
        let Some(last) = last_run else { return now };
        match self {
            Self::Every(period) => last + period,
            Self::Daily(time_of_day) => {
                let today = last - last % 86_400 + u64::from(time_of_day);
                if today > last { today } else { today + 86_400 }
            }
        }
    }
}

/// Parse an interval like `"90s"`, `"30m"`, `"1h"`, or `"1d"`.
pub fn parse_every(s: &str) -> Result<u64, String> {
    let (digits, unit) = s.split_at(s.len().saturating_sub(1));
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("invalid interval {s:?}: expected <number><s|m|h|d>"))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3_600,
        "d" => n * 86_400,
        _ => return Err(format!("invalid interval unit in {s:?}: expected s|m|h|d")),
    };
    if secs == 0 {
        return Err(format!("interval {s:?} must be positive"));
    }
    Ok(secs)
}

/// Parse a UTC time of day like `"06:00"` into seconds after midnight.
pub fn parse_daily(s: &str) -> Result<u32, String> {
    let err = || format!("invalid time {s:?}: expected HH:MM (UTC)");
    let (hh, mm) = s.split_once(':').ok_or_else(err)?;
    let hours: u32 = hh.parse().map_err(|_| err())?;
    let minutes: u32 = mm.parse().map_err(|_| err())?;
    if hh.len() != 2 || mm.len() != 2 || hours > 23 || minutes > 59 {
        return Err(err());
    }
    Ok(hours * 3_600 + minutes * 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;

    #[test]
    fn parse_every_accepts_each_unit() {
        assert_eq!(parse_every("90s").unwrap(), 90);
        assert_eq!(parse_every("30m").unwrap(), 1_800);
        assert_eq!(parse_every("1h").unwrap(), 3_600);
        assert_eq!(parse_every("1d").unwrap(), DAY);
    }

    #[test]
    fn parse_every_rejects_garbage() {
        for s in ["", "m", "10", "10x", "-5m", "0h"] {
            assert!(parse_every(s).is_err(), "{s:?} must be rejected");
        }
    }

    #[test]
    fn parse_daily_accepts_hh_mm() {
        assert_eq!(parse_daily("00:00").unwrap(), 0);
        assert_eq!(parse_daily("06:00").unwrap(), 6 * 3_600);
        assert_eq!(parse_daily("23:59").unwrap(), 23 * 3_600 + 59 * 60);
    }

    #[test]
    fn parse_daily_rejects_garbage() {
        for s in ["", "6:00", "06:0", "24:00", "12:60", "noon", "06-00"] {
            assert!(parse_daily(s).is_err(), "{s:?} must be rejected");
        }
    }

    #[test]
    fn no_recorded_run_is_due_now() {
        assert_eq!(Schedule::Every(3_600).next_due(None, 1_000), 1_000);
        assert_eq!(Schedule::Daily(0).next_due(None, 1_000), 1_000);
    }

    #[test]
    fn every_is_due_one_period_after_last_run() {
        let s = Schedule::Every(1_800);
        assert_eq!(s.next_due(Some(10_000), 10_100), 11_800);
    }

    #[test]
    fn daily_is_due_at_next_occurrence_after_last_run() {
        let six = Schedule::Daily(6 * 3_600);
        // Ran at 03:00; due today at 06:00.
        let last = 10 * DAY + 3 * 3_600;
        assert_eq!(six.next_due(Some(last), last), 10 * DAY + 6 * 3_600);
        // Ran at 06:00 exactly; due tomorrow, not again today.
        let last = 10 * DAY + 6 * 3_600;
        assert_eq!(six.next_due(Some(last), last), 11 * DAY + 6 * 3_600);
        // Ran at 09:00; due tomorrow at 06:00.
        let last = 10 * DAY + 9 * 3_600;
        assert_eq!(six.next_due(Some(last), last), 11 * DAY + 6 * 3_600);
    }

    #[test]
    fn restart_preserves_phase() {
        // A daily 06:00 duty ran; the daemon restarts at 14:00. The
        // duty is not due, and next_due is identical to what an
        // uninterrupted process would compute — cadence derives from
        // persisted state, not process start.
        let six = Schedule::Daily(6 * 3_600);
        let last = 10 * DAY + 6 * 3_600;
        let restart_now = 10 * DAY + 14 * 3_600;
        let due = six.next_due(Some(last), restart_now);
        assert_eq!(due, 11 * DAY + 6 * 3_600);
        assert!(due > restart_now, "must not fire at restart");
    }

    #[test]
    fn overdue_by_many_periods_is_one_catch_up() {
        // Down for three days: next_due is in the past (due), and
        // after the caller records last_run = now, the following
        // next_due is tomorrow — one catch-up, not three.
        let six = Schedule::Daily(6 * 3_600);
        let last = 10 * DAY + 6 * 3_600;
        let now = 13 * DAY + 12 * 3_600;
        assert!(six.next_due(Some(last), now) <= now, "must be due");
        let after_run = six.next_due(Some(now), now);
        assert_eq!(after_run, 14 * DAY + 6 * 3_600);
    }

    #[test]
    fn clock_backward_is_not_due() {
        let s = Schedule::Every(3_600);
        // now < last_run: next_due is in the (apparent) future.
        assert!(s.next_due(Some(10_000), 8_000) > 8_000);
    }
}
