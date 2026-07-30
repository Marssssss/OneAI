//! Cron job model + NL/cron/ISO schedule parsing + next-fire computation.
//!
//! `parse_schedule` accepts four schedule dialects (inspiration P2-2):
//! - **Interval**: `"30m"`, `"1h"`, `"2h30m"`, `"45s"`, `"1d"`, or `"every 2h"`
//!   / `"every 30m"`.
//! - **One-shot ISO 8601**: `"2026-08-01T09:00:00Z"` (fires once at the instant).
//! - **5-field cron**: `"*/5 * * * *"` (min hour dom month dow). Subset: `*`,
//!   `*/N`, `N`, `N,M`, `A-B`. Vixie OR/AND semantics for dom+dow. No names /
//!   `L`/`W`/`#`.
//!
//! `next_fire_after(schedule, now)` returns the next instant the schedule
//! should fire strictly after `now`, or `None` for a one-shot already past.

use std::time::Duration;

use chrono::{DateTime, Datelike, TimeZone, Timelike, Utc};

use crate::error::{CronError, Result};

/// How a fired job delivers its task into the agent (inspiration P2-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum DeliverMode {
    /// Deliver into the job's bound (originating) channel session — re-runs a
    /// turn there and relays the reply over the platform. The default; reuses
    /// the gateway's `send()` (§3.2 "deliver=origin").
    #[default]
    Origin,
    /// Run a turn silently — no platform reply (log only). For background /
    /// maintenance jobs that shouldn't surface to a user.
    Silent,
}

/// A parsed schedule.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
#[non_exhaustive]
pub enum Schedule {
    /// Repeat every interval.
    Interval {
        #[serde(with = "humantime_serde_compat")]
        interval: Duration,
    },
    /// Fire once at an instant.
    OneShot { at: DateTime<Utc> },
    /// 5-field cron expression.
    Cron { expr: String },
}

impl Schedule {
    /// The next instant strictly after `now` that this schedule fires, or
    /// `None` for a one-shot already past (or an impossible cron combo).
    pub fn next_fire_after(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Schedule::OneShot { at } if *at > now => Some(*at),
            Schedule::OneShot { .. } => None,
            Schedule::Interval { interval } => {
                let nanos = interval.as_nanos();
                if nanos == 0 {
                    return None;
                }
                let elapsed_since_epoch = now.timestamp_nanos_opt()? as u128;
                let count = elapsed_since_epoch / nanos;
                let next_epoch = (count + 1) * nanos;
                let secs = (next_epoch / 1_000_000_000) as i64;
                let nsecs = (next_epoch % 1_000_000_000) as u32;
                Utc.timestamp_opt(secs, nsecs).single()
            }
            Schedule::Cron { expr } => cron_next_fire(expr, now),
        }
    }
}

/// A cron job.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CronJob {
    /// Unique job id.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// The parsed schedule.
    pub schedule: Schedule,
    /// The task / prompt to deliver into the agent turn.
    pub task: String,
    /// The originating channel's session id to deliver into (`deliver=origin`).
    /// Empty for `Silent` jobs.
    pub session_id: String,
    /// The originating platform name + raw channel (for `Origin` delivery —
    /// the gateway needs `ChannelId` to route the reply).
    pub platform: String,
    pub channel: String,
    /// The bound DomainPack (carried via `SESSION_SOURCE` like an inbound
    /// message so the lazily-built App factory picks the right pack).
    pub pack: String,
    /// The originating user id (carried for completeness; empty for
    /// system-originated jobs).
    pub user_id: String,
    /// Delivery mode.
    #[serde(default)]
    pub deliver: DeliverMode,
    /// Next fire instant (the store's CAS point — updated atomically on fire
    /// so a restart mid-window doesn't re-fire).
    pub next_fire_at: Option<DateTime<Utc>>,
    /// Last fired instant.
    pub last_fired_at: Option<DateTime<Utc>>,
    /// Enabled flag (disabled jobs are skipped by the orchestrator).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Arbitrary metadata (callback URL for external one-shots, etc.).
    #[serde(default)]
    pub metadata: std::collections::HashMap<String, String>,
}

fn default_true() -> bool {
    true
}

impl CronJob {
    /// Build a new job with `next_fire_at` computed from the schedule relative
    /// to `now`. Id is caller-supplied (the store keeps it unique).
    pub fn new(id: impl Into<String>, name: impl Into<String>, schedule: Schedule) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            schedule,
            task: String::new(),
            session_id: String::new(),
            platform: String::new(),
            channel: String::new(),
            pack: String::new(),
            user_id: String::new(),
            deliver: DeliverMode::default(),
            next_fire_at: None,
            last_fired_at: None,
            enabled: true,
            metadata: std::collections::HashMap::new(),
        }
    }
}

// ─── parse_schedule ──────────────────────────────────────────────────────────

/// Parse a schedule string into a [`Schedule``].
pub fn parse_schedule(input: &str) -> Result<Schedule> {
    let s = input.trim();
    if s.is_empty() {
        return Err(CronError::InvalidSchedule {
            input: input.to_string(),
            message: "empty schedule".to_string(),
        });
    }

    // `every 2h` / `every 30m`
    let lower = s.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("every ") {
        let dur = parse_duration(rest).map_err(|m| CronError::InvalidSchedule {
            input: input.to_string(),
            message: m,
        })?;
        return Ok(Schedule::Interval { interval: dur });
    }

    // 5-field cron: exactly 5 whitespace-separated fields, none parseable as a
    // duration or ISO date.
    let fields: Vec<&str> = s.split_ascii_whitespace().collect();
    if fields.len() == 5 {
        validate_cron(&fields).map_err(|m| CronError::InvalidSchedule {
            input: input.to_string(),
            message: m,
        })?;
        return Ok(Schedule::Cron {
            expr: s.to_string(),
        });
    }

    // ISO 8601 one-shot datetime.
    if (s.contains('T') || s.contains(' ')) && s.contains(':') {
        if let Ok(at) = parse_iso(s) {
            return Ok(Schedule::OneShot { at });
        }
    }
    // A bare date `2026-08-01` → fire at midnight UTC that day.
    if let Ok(at) = parse_iso_date_midnight(s) {
        return Ok(Schedule::OneShot { at });
    }

    // Bare duration → interval.
    match parse_duration(s) {
        Ok(dur) => Ok(Schedule::Interval { interval: dur }),
        Err(m) => Err(CronError::InvalidSchedule {
            input: input.to_string(),
            message: m,
        }),
    }
}

fn parse_iso(s: &str) -> std::result::Result<DateTime<Utc>, String> {
    // Accept `2026-08-01T09:00:00Z`, `2026-08-01 09:00:00`, or with offset.
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|_| {
            // `YYYY-MM-DD HH:MM:SS` (space separator, no tz) → UTC.
            let n = s.replacen(' ', "T", 1) + "Z";
            chrono::DateTime::parse_from_rfc3339(&n).map(|dt| dt.with_timezone(&Utc))
        })
        .map_err(|e| format!("not an ISO datetime: {e}"))
}

fn parse_iso_date_midnight(s: &str) -> std::result::Result<DateTime<Utc>, String> {
    let n = format!("{s}T00:00:00Z");
    chrono::DateTime::parse_from_rfc3339(&n)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| format!("not a date: {e}"))
}

/// Parse a compact duration: `30m`, `1h`, `2h30m`, `45s`, `1d`, `1d2h`.
fn parse_duration(s: &str) -> std::result::Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".to_string());
    }
    let mut total: u128 = 0; // nanoseconds
    let mut digits = String::new();
    let mut seen_unit = false;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            let n: u128 = digits
                .parse()
                .map_err(|_| "non-numeric value".to_string())?;
            let mult: u128 = match ch {
                's' => 1_000_000_000,
                'm' => 60_000_000_000,
                'h' => 3_600_000_000_000,
                'd' => 86_400_000_000_000,
                _ => return Err(format!("unknown unit '{ch}'")),
            };
            total += n * mult;
            digits.clear();
            seen_unit = true;
        }
    }
    if !digits.is_empty() {
        return Err("trailing number without unit".to_string());
    }
    if !seen_unit {
        return Err("no unit (use s/m/h/d)".to_string());
    }
    u64::try_from(total)
        .map(Duration::from_nanos)
        .map_err(|_| "duration overflow".to_string())
}

// ─── cron 5-field next-fire ───────────────────────────────────────────────────

const MIN_MAX: u32 = 59;
const HOUR_MAX: u32 = 23;
const DOM_MIN: u32 = 1;
const DOM_MAX: u32 = 31;
const MON_MIN: u32 = 1;
const MON_MAX: u32 = 12;
const DOW_MAX: u32 = 6; // 0=Sunday .. 6=Saturday

/// A cron field expanded into the set of allowed values.
struct CronField {
    values: Vec<u32>,
    is_star: bool,
}

impl CronField {
    fn parse(field: &str, lo: u32, hi: u32) -> std::result::Result<Self, String> {
        if field == "*" {
            return Ok(Self {
                values: (lo..=hi).collect(),
                is_star: true,
            });
        }
        let mut out = Vec::new();
        for part in field.split(',') {
            if let Some(step_part) = part.strip_prefix("*/") {
                let step: u32 = step_part.parse().map_err(|_| "bad step")?;
                if step == 0 {
                    return Err("zero step".to_string());
                }
                out.extend((lo..=hi).step_by(step as usize));
            } else if let Some((a, b)) = part.split_once('-') {
                let a: u32 = a.parse().map_err(|_| "bad range start")?;
                let b: u32 = b.parse().map_err(|_| "bad range end")?;
                if a > b || a < lo || b > hi {
                    return Err(format!("range {a}-{b} out of {lo}-{hi}"));
                }
                out.extend(a..=b);
            } else {
                let v: u32 = part.parse().map_err(|_| "bad value")?;
                if v < lo || v > hi {
                    return Err(format!("value {v} out of {lo}-{hi}"));
                }
                out.push(v);
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(Self {
            values: out,
            is_star: false,
        })
    }

    fn contains(&self, v: u32) -> bool {
        self.values.binary_search(&v).is_ok()
    }
}

fn validate_cron(fields: &[&str]) -> std::result::Result<(), String> {
    CronField::parse(fields[0], 0, MIN_MAX)?;
    CronField::parse(fields[1], 0, HOUR_MAX)?;
    CronField::parse(fields[2], DOM_MIN, DOM_MAX)?;
    CronField::parse(fields[3], MON_MIN, MON_MAX)?;
    // 7 is also accepted for Sunday (cron allows 0 and 7).
    let dow_field = normalize_dow(fields[4])?;
    let _ = CronField::parse(&dow_field, 0, DOW_MAX);
    Ok(())
}

/// Normalize cron `7` (Sunday) → `0` in the dow field.
fn normalize_dow(field: &str) -> std::result::Result<String, String> {
    let mut out = String::new();
    for part in field.split(',') {
        if out.is_empty() {
        } else {
            out.push(',');
        }
        if part == "7" {
            out.push('0');
        } else if let Some((a, b)) = part.split_once('-') {
            let a = if a == "7" {
                "0".to_string()
            } else {
                a.to_string()
            };
            let b = if b == "7" {
                "0".to_string()
            } else {
                b.to_string()
            };
            out.push_str(&a);
            out.push('-');
            out.push_str(&b);
        } else {
            out.push_str(part);
        }
    }
    Ok(out)
}

fn cron_next_fire(expr: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let fields: Vec<&str> = expr.split_ascii_whitespace().collect();
    if fields.len() != 5 {
        return None;
    }
    let min = CronField::parse(fields[0], 0, MIN_MAX).ok()?;
    let hour = CronField::parse(fields[1], 0, HOUR_MAX).ok()?;
    let dom = CronField::parse(fields[2], DOM_MIN, DOM_MAX).ok()?;
    let mon = CronField::parse(fields[3], MON_MIN, MON_MAX).ok()?;
    let dow_field = normalize_dow(fields[4]).ok()?;
    let dow = CronField::parse(&dow_field, 0, DOW_MAX).ok()?;

    // Vixie semantics: if BOTH dom and dow are restricted (non-*), fire when
    // EITHER matches; otherwise both must match.
    let both_restricted = !dom.is_star && !dow.is_star;

    // Start from the minute strictly after `now`, truncated to the minute.
    let mut t = Utc
        .with_ymd_and_hms(
            now.year(),
            now.month(),
            now.day(),
            now.hour(),
            now.minute(),
            0,
        )
        .single()?
        + Duration::from_secs(60);

    // Cap: ~5 years of minute steps — an impossible combo (Feb 31 weekday) is
    // detectable well within.
    for _ in 0..(5 * 365 * 24 * 60) {
        // Month not in set → jump to first day of next valid month at 00:00.
        if !mon.contains(t.month()) {
            let mut m = t.month() + 1;
            let mut y = t.year();
            if m > 12 {
                m = 1;
                y += 1;
            }
            while !mon.contains(m) {
                m += 1;
                if m > 12 {
                    m = 1;
                    y += 1;
                }
            }
            t = Utc.with_ymd_and_hms(y, m, 1, 0, 0, 0).single()?;
            continue;
        }
        let dow_val = weekday_to_cron(t.weekday());
        let dom_match = dom.contains(t.day());
        let dow_match = dow.contains(dow_val);
        let fire_day = if both_restricted {
            dom_match || dow_match
        } else {
            dom_match && dow_match
        };
        if !fire_day {
            // Advance to next day, 00:00.
            let next_date = t.naive_utc().date().succ_opt()?;
            t = DateTime::<Utc>::from_naive_utc_and_offset(next_date.and_hms_opt(0, 0, 0)?, Utc);
            continue;
        }
        if !hour.contains(t.hour()) {
            // Next hour, minute 0.
            t += Duration::from_secs(3600);
            // Truncate to hour.
            t = Utc
                .with_ymd_and_hms(t.year(), t.month(), t.day(), t.hour(), 0, 0)
                .single()?;
            continue;
        }
        if !min.contains(t.minute()) {
            t += Duration::from_secs(60);
            continue;
        }
        return Some(t);
    }
    None
}

/// Map chrono Weekday → cron dow (0=Sunday .. 6=Saturday).
fn weekday_to_cron(w: chrono::Weekday) -> u32 {
    use chrono::Weekday::*;
    match w {
        Sun => 0,
        Mon => 1,
        Tue => 2,
        Wed => 3,
        Thu => 4,
        Fri => 5,
        Sat => 6,
    }
}

// ─── serde compat for Duration (storing as nanoseconds) ───────────────────────

/// Serialize `Duration` as nanoseconds (u64), deserialize back. Avoids pulling
/// `humantime`/`humantime_serde` into the supply chain for one field.
mod humantime_serde_compat {
    use std::time::Duration;

    use serde::{Deserialize, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_nanos() as u64)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let n = u64::deserialize(d)?;
        Ok(Duration::from_nanos(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-07-30T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn parse_interval_variants() {
        assert!(matches!(
            parse_schedule("30m").unwrap(),
            Schedule::Interval { .. }
        ));
        assert!(matches!(
            parse_schedule("every 2h").unwrap(),
            Schedule::Interval { .. }
        ));
        assert!(matches!(
            parse_schedule("2h30m").unwrap(),
            Schedule::Interval { .. }
        ));
    }

    #[test]
    fn parse_one_shot_iso() {
        let s = parse_schedule("2026-08-01T09:00:00Z").unwrap();
        match s {
            Schedule::OneShot { at } => assert_eq!(at.to_rfc3339(), "2026-08-01T09:00:00+00:00"),
            _ => panic!("expected OneShot"),
        }
    }

    #[test]
    fn parse_cron_5_field() {
        assert!(matches!(
            parse_schedule("*/5 * * * *").unwrap(),
            Schedule::Cron { .. }
        ));
        assert!(matches!(
            parse_schedule("0 9 * * *").unwrap(),
            Schedule::Cron { .. }
        ));
        // 6-field is NOT supported (rejects).
        assert!(parse_schedule("* * * * * *").is_err());
    }

    #[test]
    fn interval_next_fire_strictly_after() {
        let s = Schedule::Interval {
            interval: Duration::from_secs(60),
        };
        let n = now();
        let next = s.next_fire_after(n).unwrap();
        assert!(next > n);
        // Next fire is within 60s of now (boundary-aligned to the minute).
        assert!((next - n).num_seconds() <= 60);
    }

    #[test]
    fn one_shot_past_returns_none() {
        let past = Schedule::OneShot {
            at: now() - Duration::from_secs(3600),
        };
        assert!(past.next_fire_after(now()).is_none());
    }

    #[test]
    fn cron_every_5_minutes_next_fire() {
        let s = parse_schedule("*/5 * * * *").unwrap();
        let next = s.next_fire_after(now()).unwrap();
        assert_eq!(next.minute() % 5, 0);
        assert!(next > now());
    }

    #[test]
    fn cron_daily_9am_next_fire() {
        let s = parse_schedule("0 9 * * *").unwrap();
        let next = s.next_fire_after(now()).unwrap();
        assert_eq!(next.hour(), 9);
        assert_eq!(next.minute(), 0);
        assert!(next > now());
    }

    #[test]
    fn cron_dow_7_normalized_to_sunday() {
        // `0 0 * * 7` → Sunday midnight. Next Sunday after 2026-07-30 (Thu).
        let s = parse_schedule("0 0 * * 7").unwrap();
        let next = s.next_fire_after(now()).unwrap();
        assert_eq!(next.weekday(), chrono::Weekday::Sun);
        assert_eq!(next.hour(), 0);
    }

    #[test]
    fn cron_dom_and_dow_both_restricted_or_semantics() {
        // `0 0 15 * 1` → fires on the 15th OR Monday. 2026-07-30 is Thursday;
        // next match is Monday 2026-08-03 (before the 15th).
        let s = parse_schedule("0 0 15 * 1").unwrap();
        let next = s.next_fire_after(now()).unwrap();
        assert_eq!(next.weekday(), chrono::Weekday::Mon);
        assert_eq!(next.day(), 3);
    }

    #[test]
    fn duration_parser_units() {
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(30 * 60));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(
            parse_duration("2h30m").unwrap(),
            Duration::from_secs(2 * 3600 + 30 * 60)
        );
        assert!(parse_duration("1").is_err());
        assert!(parse_duration("30x").is_err());
    }
}
