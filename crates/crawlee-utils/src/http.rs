//! HTTP header helpers.

use std::time::Duration;

use chrono::{DateTime, Utc};

/// The delay a `Retry-After` header asks for: whole seconds, or an HTTP-date. Like
/// `parseRetryAfterHeader` in Crawlee for JS, `None` when the header is missing or unreadable,
/// and also when the delay is zero or already over. A zero delay would count as a rate limit
/// without holding the domain back.
pub fn parse_retry_after(value: Option<&str>, now: DateTime<Utc>) -> Option<Duration> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    if value.bytes().all(|b| b.is_ascii_digit()) {
        let seconds: u64 = value.parse().ok()?;
        return (seconds > 0).then(|| Duration::from_secs(seconds));
    }
    let date = DateTime::parse_from_rfc2822(value).or_else(|_| DateTime::parse_from_rfc3339(value)).ok()?;
    (date.with_timezone(&Utc) - now).to_std().ok().filter(|delay| !delay.is_zero())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_and_dates() {
        let now = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(parse_retry_after(Some(" 120 "), now), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after(Some("0"), now), None);
        assert_eq!(parse_retry_after(Some("1.5"), now), None);
        assert_eq!(parse_retry_after(Some("Thu, 01 Jan 2026 00:00:30 GMT"), now), Some(Duration::from_secs(30)));
        assert_eq!(parse_retry_after(Some("Wed, 21 Oct 2015 07:28:00 GMT"), now), None);
        assert_eq!(parse_retry_after(None, now), None);
    }
}
