//! Small cross-cutting helpers.

use chrono::{DateTime, SecondsFormat, Utc};

/// Current UTC time formatted the way the schema stores it (`2026-10-14T06:56:37Z`).
pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Same, but offset by a number of minutes (used for expiring rows).
pub fn rfc3339_in_minutes(minutes: i64) -> String {
    (Utc::now() + chrono::Duration::minutes(minutes)).to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Same, but offset by a number of days (used for token expiry).
pub fn rfc3339_in_days(days: i64) -> String {
    (Utc::now() + chrono::Duration::days(days)).to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// True when `timestamp` (as stored by `now_rfc3339`) lies in the past.
pub fn is_expired(timestamp: &str) -> bool {
    match DateTime::parse_from_rfc3339(timestamp) {
        Ok(ts) => ts < Utc::now(),
        Err(_) => false,
    }
}
