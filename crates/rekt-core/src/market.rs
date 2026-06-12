//! US equity market hours, in the only timezone that matters for them.
//!
//! Approximate by design for now: regular session Mon–Fri 9:30–16:00
//! America/New_York. Exchange holidays and half-days are NOT modeled yet —
//! on those days REKT will say "open" while quotes sit still. The UI shows
//! quote timestamps, so the lie is at least visible. Holiday calendar is
//! tracked for Phase 2 (order tickets must not pretend the market is open).

use chrono::{DateTime, Datelike, NaiveTime, Utc, Weekday};
use chrono_tz::America::New_York;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketStatus {
    Open,
    Closed,
}

pub fn us_market_status(now: DateTime<Utc>) -> MarketStatus {
    let local = now.with_timezone(&New_York);
    let weekday = matches!(
        local.weekday(),
        Weekday::Mon | Weekday::Tue | Weekday::Wed | Weekday::Thu | Weekday::Fri
    );
    let open = NaiveTime::from_hms_opt(9, 30, 0).unwrap();
    let close = NaiveTime::from_hms_opt(16, 0, 0).unwrap();
    let in_session = local.time() >= open && local.time() < close;

    if weekday && in_session {
        MarketStatus::Open
    } else {
        MarketStatus::Closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn weekday_midday_is_open() {
        // 2026-06-12 is a Friday; 15:00 UTC = 11:00 EDT.
        assert_eq!(
            us_market_status(at("2026-06-12T15:00:00Z")),
            MarketStatus::Open
        );
    }

    #[test]
    fn weekday_evening_and_weekend_are_closed() {
        // 21:00 UTC = 17:00 EDT, after close.
        assert_eq!(
            us_market_status(at("2026-06-12T21:00:00Z")),
            MarketStatus::Closed
        );
        // Saturday.
        assert_eq!(
            us_market_status(at("2026-06-13T15:00:00Z")),
            MarketStatus::Closed
        );
    }

    #[test]
    fn pre_open_minute_boundary() {
        // 13:29 UTC = 9:29 EDT → closed; 13:30 UTC = 9:30 EDT → open.
        assert_eq!(
            us_market_status(at("2026-06-12T13:29:59Z")),
            MarketStatus::Closed
        );
        assert_eq!(
            us_market_status(at("2026-06-12T13:30:00Z")),
            MarketStatus::Open
        );
    }
}
