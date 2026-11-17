//! UTC timestamps for DocMonk's records and logs, backed by `jiff`.
//!
//! This replaces the small slice of `chrono` DocMonk used (RUSTSEC-2026-0543:
//! chrono is unmaintained). The names follow that slice (`Utc::now()`,
//! `DateTime<Utc>`, `to_rfc3339_opts`, `Duration::seconds`, ...) so call
//! sites read the same, and every text format is byte-identical to what
//! chrono wrote: stored timestamps, lease records and log lines compare and
//! sort exactly as before.
//!
//! Everything is UTC. A parsed RFC 3339 value keeps no offset: it is the
//! same instant in UTC, which is all DocMonk ever does with one.

use std::fmt;
use std::marker::PhantomData;
use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};

/// The UTC time zone marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Utc;

/// The marker of a value parsed from text with an explicit offset. It is
/// already normalized to UTC; [`DateTime::with_timezone`] only relabels it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct FixedOffset;

/// An instant with nanosecond precision.
pub struct DateTime<Tz = Utc> {
    ts: jiff::Timestamp,
    tz: PhantomData<Tz>,
}

// Hand-written so they hold for every marker type (derives would require
// `Tz: Copy` and so on).
impl<Tz> Clone for DateTime<Tz> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<Tz> Copy for DateTime<Tz> {}
impl<A, B> PartialEq<DateTime<B>> for DateTime<A> {
    fn eq(&self, other: &DateTime<B>) -> bool {
        self.ts == other.ts
    }
}
impl<Tz> Eq for DateTime<Tz> {}
impl<A, B> PartialOrd<DateTime<B>> for DateTime<A> {
    fn partial_cmp(&self, other: &DateTime<B>) -> Option<std::cmp::Ordering> {
        Some(self.ts.cmp(&other.ts))
    }
}
impl<Tz> Ord for DateTime<Tz> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.ts.cmp(&other.ts)
    }
}
impl<Tz> std::hash::Hash for DateTime<Tz> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.ts.hash(state);
    }
}

impl<Tz> fmt::Debug for DateTime<Tz> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rfc3339_opts(SecondsFormat::AutoSi, true))
    }
}

impl<Tz> fmt::Display for DateTime<Tz> {
    /// `2026-01-02 03:04:05.123456 UTC`, as chrono displayed a UTC value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = self.civil();
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            c.year(),
            c.month(),
            c.day(),
            c.hour(),
            c.minute(),
            c.second()
        )?;
        let ns = self.subsec_nanos();
        if ns != 0 {
            write!(f, "{}", auto_fraction(ns))?;
        }
        f.write_str(" UTC")
    }
}

/// How many fractional-second digits RFC 3339 output carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecondsFormat {
    Secs,
    Millis,
    Micros,
    Nanos,
    /// The shortest of none, 3, 6 or 9 digits that loses nothing.
    AutoSi,
}

/// A text value that is not a valid timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid timestamp: {}", self.0)
    }
}

impl std::error::Error for ParseError {}

impl Utc {
    /// The current instant.
    pub fn now() -> DateTime<Utc> {
        DateTime::from_ts(jiff::Timestamp::now())
    }

    /// `Utc.with_ymd_and_hms(..)`, as in chrono's `TimeZone`: `None` when the
    /// fields do not name a valid UTC time.
    pub fn with_ymd_and_hms(
        &self,
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        min: u32,
        sec: u32,
    ) -> LocalResult {
        let dt = (|| {
            let d = jiff::civil::DateTime::new(
                i16::try_from(year).ok()?,
                i8::try_from(month).ok()?,
                i8::try_from(day).ok()?,
                i8::try_from(hour).ok()?,
                i8::try_from(min).ok()?,
                i8::try_from(sec).ok()?,
                0,
            )
            .ok()?;
            let ts = jiff::tz::Offset::UTC.to_timestamp(d).ok()?;
            Some(DateTime::from_ts(ts))
        })();
        LocalResult(dt)
    }
}

/// The result of building a time from fields (UTC has no ambiguity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalResult(Option<DateTime<Utc>>);

impl LocalResult {
    pub fn single(self) -> Option<DateTime<Utc>> {
        self.0
    }

    pub fn unwrap(self) -> DateTime<Utc> {
        self.0.expect("no such UTC time")
    }
}

impl<Tz> DateTime<Tz> {
    fn from_ts(ts: jiff::Timestamp) -> Self {
        Self {
            ts,
            tz: PhantomData,
        }
    }

    fn civil(&self) -> jiff::civil::DateTime {
        jiff::tz::Offset::UTC.to_datetime(self.ts)
    }

    fn subsec_nanos(&self) -> u32 {
        // `subsec_nanosecond` is negative before 1970; the civil fraction
        // is always the non-negative remainder.
        let n = self.ts.subsec_nanosecond();
        if n < 0 {
            (n + 1_000_000_000) as u32
        } else {
            n as u32
        }
    }

    /// The same instant labelled as UTC.
    pub fn with_timezone<T>(&self, _tz: &T) -> DateTime<Utc> {
        DateTime::from_ts(self.ts)
    }

    /// Seconds since the Unix epoch (floored).
    pub fn timestamp(&self) -> i64 {
        self.ts.as_second() - i64::from(self.ts.subsec_nanosecond() < 0)
    }

    /// Microseconds within the second (0..=999_999).
    pub fn timestamp_subsec_micros(&self) -> u32 {
        self.subsec_nanos() / 1_000
    }

    /// Milliseconds since the Unix epoch (floored).
    pub fn timestamp_millis(&self) -> i64 {
        self.timestamp() * 1000 + i64::from(self.subsec_nanos() / 1_000_000)
    }

    /// RFC 3339 text: `2026-01-02T03:04:05.123456Z` (`use_z`) or
    /// `...+00:00`.
    pub fn to_rfc3339_opts(&self, secform: SecondsFormat, use_z: bool) -> String {
        let c = self.civil();
        let mut s = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            c.year(),
            c.month(),
            c.day(),
            c.hour(),
            c.minute(),
            c.second()
        );
        let ns = self.subsec_nanos();
        match secform {
            SecondsFormat::Secs => {}
            SecondsFormat::Millis => s.push_str(&format!(".{:03}", ns / 1_000_000)),
            SecondsFormat::Micros => s.push_str(&format!(".{:06}", ns / 1_000)),
            SecondsFormat::Nanos => s.push_str(&format!(".{ns:09}")),
            SecondsFormat::AutoSi => {
                if ns != 0 {
                    s.push_str(&auto_fraction(ns));
                }
            }
        }
        s.push_str(if use_z { "Z" } else { "+00:00" });
        s
    }

    /// `to_rfc3339_opts(AutoSi, false)`, as chrono's `to_rfc3339`.
    pub fn to_rfc3339(&self) -> String {
        self.to_rfc3339_opts(SecondsFormat::AutoSi, false)
    }

    /// strftime-style formatting for the specifiers DocMonk uses:
    /// `%Y %m %d %H %M %S` (zero-padded), `%f` (nanoseconds, 9 digits),
    /// `%.3f` / `%.6f` / `%.9f` (a dot and that many digits), `%.f` (the
    /// shortest of 3/6/9 digits, nothing when zero) and `%%`. Any other
    /// character is copied as is.
    pub fn format(&self, pattern: &str) -> String {
        let c = self.civil();
        let ns = self.subsec_nanos();
        let mut out = String::with_capacity(pattern.len() + 16);
        let mut chars = pattern.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '%' {
                out.push(ch);
                continue;
            }
            match chars.next() {
                Some('Y') => out.push_str(&format!("{:04}", c.year())),
                Some('m') => out.push_str(&format!("{:02}", c.month())),
                Some('d') => out.push_str(&format!("{:02}", c.day())),
                Some('H') => out.push_str(&format!("{:02}", c.hour())),
                Some('M') => out.push_str(&format!("{:02}", c.minute())),
                Some('S') => out.push_str(&format!("{:02}", c.second())),
                Some('f') => out.push_str(&format!("{ns:09}")),
                Some('%') => out.push('%'),
                Some('.') => match chars.next() {
                    Some('3') if chars.next_if_eq(&'f').is_some() => {
                        out.push_str(&format!(".{:03}", ns / 1_000_000))
                    }
                    Some('6') if chars.next_if_eq(&'f').is_some() => {
                        out.push_str(&format!(".{:06}", ns / 1_000))
                    }
                    Some('9') if chars.next_if_eq(&'f').is_some() => {
                        out.push_str(&format!(".{ns:09}"))
                    }
                    Some('f') => {
                        if ns != 0 {
                            out.push_str(&auto_fraction(ns));
                        }
                    }
                    other => panic!("unsupported time format specifier %.{other:?} in {pattern:?}"),
                },
                other => panic!("unsupported time format specifier %{other:?} in {pattern:?}"),
            }
        }
        out
    }

    pub fn checked_add_signed(self, d: Duration) -> Option<Self> {
        self.ts.checked_add(d.0).ok().map(Self::from_ts)
    }

    pub fn checked_sub_signed(self, d: Duration) -> Option<Self> {
        self.ts.checked_sub(d.0).ok().map(Self::from_ts)
    }

    pub fn signed_duration_since<T>(self, other: DateTime<T>) -> Duration {
        Duration(self.ts.duration_since(other.ts))
    }
}

/// `.123`, `.123456` or `.123456789`: the shortest that keeps every digit.
fn auto_fraction(ns: u32) -> String {
    if ns.is_multiple_of(1_000_000) {
        format!(".{:03}", ns / 1_000_000)
    } else if ns.is_multiple_of(1_000) {
        format!(".{:06}", ns / 1_000)
    } else {
        format!(".{ns:09}")
    }
}

impl DateTime<FixedOffset> {
    /// Parses RFC 3339 text with an offset (`Z` or `±HH:MM`), as chrono's
    /// `DateTime::parse_from_rfc3339`: `T`, `t` or a space between date and
    /// time, any number of fractional digits.
    pub fn parse_from_rfc3339(s: &str) -> Result<Self, ParseError> {
        parse_rfc3339(s)
            .map(Self::from_ts)
            .ok_or_else(|| ParseError(s.to_owned()))
    }
}

impl DateTime<Utc> {
    /// The instant `ms` milliseconds after the Unix epoch, if representable.
    pub fn from_timestamp_millis(ms: i64) -> Option<Self> {
        jiff::Timestamp::from_millisecond(ms)
            .ok()
            .map(Self::from_ts)
    }

    /// The instant `secs` seconds and `nanos` nanoseconds after the epoch.
    pub fn from_timestamp(secs: i64, nanos: u32) -> Option<Self> {
        if nanos >= 1_000_000_000 {
            return None;
        }
        jiff::Timestamp::new(secs, nanos as i32)
            .ok()
            .map(Self::from_ts)
    }
}

fn digits(s: &[u8]) -> Option<i64> {
    if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(s).ok()?.parse().ok()
}

/// `YYYY-MM-DD[Tt ]HH:MM:SS[.fff…]` into civil fields and the nanosecond
/// fraction; the rest of the input is returned.
fn parse_civil(b: &[u8]) -> Option<(jiff::civil::DateTime, &[u8])> {
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't' | b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let year = digits(&b[0..4])?;
    let month = digits(&b[5..7])?;
    let day = digits(&b[8..10])?;
    let hour = digits(&b[11..13])?;
    let minute = digits(&b[14..16])?;
    let second = digits(&b[17..19])?;
    let mut rest = &b[19..];
    let mut nanos = 0i64;
    if let Some(frac) = rest.strip_prefix(b".") {
        let n = frac.iter().take_while(|c| c.is_ascii_digit()).count();
        if n == 0 {
            return None;
        }
        // Digits past the ninth are truncated, as chrono did.
        let kept = &frac[..n.min(9)];
        nanos = digits(kept)? * 10i64.pow(9 - kept.len() as u32);
        rest = &frac[n..];
    }
    // A leap second (`:60`) is the last nanosecond of its minute.
    let (second, nanos) = if second == 60 {
        (59, 999_999_999)
    } else {
        (second, nanos)
    };
    let dt = jiff::civil::DateTime::new(
        year as i16,
        month as i8,
        day as i8,
        hour as i8,
        minute as i8,
        second as i8,
        nanos as i32,
    )
    .ok()?;
    Some((dt, rest))
}

fn parse_rfc3339(s: &str) -> Option<jiff::Timestamp> {
    let (dt, rest) = parse_civil(s.as_bytes())?;
    let offset_secs: i32 = match rest {
        b"Z" | b"z" => 0,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] => {
            let h = digits(&[*h1, *h2])? as i32;
            let m = digits(&[*m1, *m2])? as i32;
            if h > 23 || m > 59 {
                return None;
            }
            let v = h * 3600 + m * 60;
            if *sign == b'-' {
                -v
            } else {
                v
            }
        }
        _ => return None,
    };
    let offset = jiff::tz::Offset::from_seconds(offset_secs).ok()?;
    offset.to_timestamp(dt).ok()
}

/// A date and time without an offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NaiveDateTime(jiff::civil::DateTime);

impl NaiveDateTime {
    /// Parses `%Y-%m-%dT%H:%M:%S%.f` (the only pattern DocMonk reads): a
    /// `T` separator, then an optional fraction.
    pub fn parse_from_str(s: &str, pattern: &str) -> Result<Self, ParseError> {
        assert_eq!(
            pattern, "%Y-%m-%dT%H:%M:%S%.f",
            "unsupported NaiveDateTime pattern"
        );
        let b = s.as_bytes();
        match parse_civil(b) {
            Some((dt, rest)) if rest.is_empty() && b.get(10) == Some(&b'T') => Ok(Self(dt)),
            _ => Err(ParseError(s.to_owned())),
        }
    }

    /// This date and time read as UTC.
    pub fn and_utc(&self) -> DateTime<Utc> {
        DateTime::from_ts(
            jiff::tz::Offset::UTC
                .to_timestamp(self.0)
                .expect("a civil time in range is a timestamp in range"),
        )
    }
}

/// A signed span of time with nanosecond precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Duration(jiff::SignedDuration);

impl Duration {
    pub const fn seconds(secs: i64) -> Self {
        Self(jiff::SignedDuration::from_secs(secs))
    }

    pub const fn minutes(mins: i64) -> Self {
        Self(jiff::SignedDuration::from_secs(mins * 60))
    }

    pub const fn hours(hours: i64) -> Self {
        Self(jiff::SignedDuration::from_secs(hours * 3600))
    }

    pub const fn milliseconds(ms: i64) -> Self {
        Self(jiff::SignedDuration::from_millis(ms))
    }

    pub const fn microseconds(us: i64) -> Self {
        Self(jiff::SignedDuration::from_micros(us))
    }

    pub const fn zero() -> Self {
        Self(jiff::SignedDuration::ZERO)
    }

    /// A `std` duration, if it fits.
    pub fn from_std(d: std::time::Duration) -> Result<Self, ParseError> {
        jiff::SignedDuration::try_from(d)
            .map(Self)
            .map_err(|e| ParseError(e.to_string()))
    }

    /// The `std` duration, when not negative.
    pub fn to_std(&self) -> Result<std::time::Duration, ParseError> {
        std::time::Duration::try_from(self.0).map_err(|e| ParseError(e.to_string()))
    }

    /// Whole seconds, truncated toward zero.
    pub fn num_seconds(&self) -> i64 {
        self.0.as_secs()
    }

    /// Whole milliseconds, truncated toward zero.
    pub fn num_milliseconds(&self) -> i64 {
        self.0.as_millis() as i64
    }

    pub fn as_seconds_f64(&self) -> f64 {
        self.0.as_secs_f64()
    }

    pub fn checked_mul(&self, rhs: i32) -> Option<Self> {
        self.0.checked_mul(rhs).map(Self)
    }
}

impl Add for Duration {
    type Output = Duration;
    fn add(self, rhs: Duration) -> Duration {
        Duration(self.0 + rhs.0)
    }
}

impl Sub for Duration {
    type Output = Duration;
    fn sub(self, rhs: Duration) -> Duration {
        Duration(self.0 - rhs.0)
    }
}

impl Neg for Duration {
    type Output = Duration;
    fn neg(self) -> Duration {
        Duration(-self.0)
    }
}

impl std::ops::Mul<i32> for Duration {
    type Output = Duration;
    fn mul(self, rhs: i32) -> Duration {
        Duration(self.0 * rhs)
    }
}

impl<Tz> Add<Duration> for DateTime<Tz> {
    type Output = DateTime<Tz>;
    fn add(self, rhs: Duration) -> Self {
        self.checked_add_signed(rhs)
            .expect("timestamp out of range")
    }
}

impl<Tz> Sub<Duration> for DateTime<Tz> {
    type Output = DateTime<Tz>;
    fn sub(self, rhs: Duration) -> Self {
        self.checked_sub_signed(rhs)
            .expect("timestamp out of range")
    }
}

impl<Tz> AddAssign<Duration> for DateTime<Tz> {
    fn add_assign(&mut self, rhs: Duration) {
        *self = *self + rhs;
    }
}

impl<Tz> SubAssign<Duration> for DateTime<Tz> {
    fn sub_assign(&mut self, rhs: Duration) {
        *self = *self - rhs;
    }
}

impl<A, B> Sub<DateTime<B>> for DateTime<A> {
    type Output = Duration;
    fn sub(self, rhs: DateTime<B>) -> Duration {
        self.signed_duration_since(rhs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64, nanos: u32) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, nanos).unwrap()
    }

    // Expected strings are chrono 0.4's output for the same instants.
    #[test]
    fn rfc3339_output_matches_chrono() {
        let t = at(1_767_323_045, 123_456_789); // 2026-01-02T03:04:05.123456789Z
        assert_eq!(
            t.to_rfc3339_opts(SecondsFormat::Secs, true),
            "2026-01-02T03:04:05Z"
        );
        assert_eq!(
            t.to_rfc3339_opts(SecondsFormat::Secs, false),
            "2026-01-02T03:04:05+00:00"
        );
        assert_eq!(
            t.to_rfc3339_opts(SecondsFormat::Millis, true),
            "2026-01-02T03:04:05.123Z"
        );
        assert_eq!(
            t.to_rfc3339_opts(SecondsFormat::Micros, false),
            "2026-01-02T03:04:05.123456+00:00"
        );
        assert_eq!(
            t.to_rfc3339_opts(SecondsFormat::Micros, true),
            "2026-01-02T03:04:05.123456Z"
        );
        assert_eq!(
            t.to_rfc3339_opts(SecondsFormat::Nanos, true),
            "2026-01-02T03:04:05.123456789Z"
        );
        let whole = at(1_767_323_045, 0);
        assert_eq!(
            whole.to_rfc3339_opts(SecondsFormat::Micros, false),
            "2026-01-02T03:04:05.000000+00:00"
        );
        assert_eq!(whole.to_rfc3339(), "2026-01-02T03:04:05+00:00");
        assert_eq!(
            at(1_767_323_045, 120_000_000).to_rfc3339(),
            "2026-01-02T03:04:05.120+00:00"
        );
        assert_eq!(
            at(0, 0).to_rfc3339_opts(SecondsFormat::Secs, true),
            "1970-01-01T00:00:00Z"
        );
        assert_eq!(
            at(-1, 500_000_000).to_rfc3339_opts(SecondsFormat::Millis, true),
            "1969-12-31T23:59:59.500Z"
        );
    }

    #[test]
    fn strftime_patterns_match_chrono() {
        let t = at(1_767_323_045, 7_000_123);
        assert_eq!(t.format("%Y%m%d%H%M%S%f"), "20260102030405007000123");
        assert_eq!(t.format("%Y%m%dT%H%M%S%.3fZ"), "20260102T030405.007Z");
        assert_eq!(
            t.format("%Y-%m-%d %H:%M:%S%.f"),
            "2026-01-02 03:04:05.007000123"
        );
        assert_eq!(at(1_767_323_045, 0).format("%H:%M:%S%.f|%%"), "03:04:05|%");
        assert_eq!(t.to_string(), "2026-01-02 03:04:05.007000123 UTC");
    }

    #[test]
    fn rfc3339_parsing_matches_chrono() {
        let p = |s: &str| DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc));
        assert_eq!(p("2026-01-02T03:04:05Z").unwrap(), at(1_767_323_045, 0));
        assert_eq!(
            p("2026-01-02t03:04:05.5z").unwrap(),
            at(1_767_323_045, 500_000_000)
        );
        assert_eq!(
            p("2026-01-02 05:04:05+02:00").unwrap(),
            at(1_767_323_045, 0)
        );
        assert_eq!(
            p("2026-01-01T22:04:05.123456-05:00").unwrap(),
            at(1_767_323_045, 123_456_000)
        );
        assert_eq!(
            p("2026-01-02T03:04:05.1234567891Z").unwrap(),
            at(1_767_323_045, 123_456_789)
        );
        for bad in [
            "",
            "2026-01-02",
            "2026-01-02T03:04:05",
            "2026-13-02T03:04:05Z",
            "2026-01-02T03:04:05+0200",
            "2026-01-02T03:04:05.Z",
            "x026-01-02T03:04:05Z",
        ] {
            assert!(p(bad).is_err(), "{bad}");
        }
        // Round trip of what DocMonk writes.
        let t = at(1_767_323_045, 123_456_000);
        assert_eq!(
            p(&t.to_rfc3339_opts(SecondsFormat::Micros, false)).unwrap(),
            t
        );
    }

    #[test]
    fn naive_parsing_and_epoch_helpers() {
        let n = NaiveDateTime::parse_from_str("2026-01-02T03:04:05.25", "%Y-%m-%dT%H:%M:%S%.f")
            .unwrap();
        assert_eq!(n.and_utc(), at(1_767_323_045, 250_000_000));
        assert!(
            NaiveDateTime::parse_from_str("2026-01-02 03:04:05", "%Y-%m-%dT%H:%M:%S%.f").is_err()
        );
        assert!(
            NaiveDateTime::parse_from_str("2026-01-02T03:04:05Z", "%Y-%m-%dT%H:%M:%S%.f").is_err()
        );
        let t = DateTime::from_timestamp_millis(1_767_323_045_123).unwrap();
        assert_eq!(t.timestamp_millis(), 1_767_323_045_123);
        assert_eq!(t.timestamp(), 1_767_323_045);
        assert_eq!(at(-1, 500_000_000).timestamp(), -1);
        assert_eq!(at(-1, 500_000_000).timestamp_millis(), -500);
        assert_eq!(at(-2, 999_000_000).timestamp_millis(), -1001);
        assert_eq!(
            Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap(),
            at(1_767_323_045, 0)
        );
        assert!(Utc
            .with_ymd_and_hms(2026, 2, 30, 0, 0, 0)
            .single()
            .is_none());
    }

    #[test]
    fn durations_and_arithmetic() {
        const D: Duration = Duration::seconds(30);
        let t = at(1_767_323_045, 0);
        assert_eq!((t + D) - t, D);
        assert_eq!((t - Duration::hours(1)).timestamp(), 1_767_323_045 - 3600);
        assert_eq!(Duration::milliseconds(1500).num_seconds(), 1);
        assert_eq!(Duration::milliseconds(-1500).num_seconds(), -1);
        assert_eq!(Duration::microseconds(2500).num_milliseconds(), 2);
        assert_eq!(Duration::seconds(3).as_seconds_f64(), 3.0);
        assert_eq!(
            Duration::from_std(std::time::Duration::from_millis(250)).unwrap(),
            Duration::milliseconds(250)
        );
        assert_eq!(D * 4, Duration::minutes(2));
        assert!(Duration::seconds(-1).to_std().is_err());
        assert!(Duration::seconds(1) < Duration::minutes(1));
    }
}
