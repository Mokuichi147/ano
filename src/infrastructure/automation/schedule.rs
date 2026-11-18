//! The source `schedule`: events that `ano serve` makes by itself at the
//! times of a cron pattern, such as a morning check-in or a weekly review.
//!
//! The pattern has five fields (minute, hour, day of month, month, day of
//! week) and is read in the server's local time zone. Nothing can be posted
//! to a schedule.

use super::options;
use crate::{
    application::automation::{EventSource, KindSpec, PostedRequest, Rejection},
    domain::automation::{AutomationEvent, Trust},
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, TimeZone};
use croner::{
    parser::{CronParser, Seconds},
    Cron,
};
use serde::Deserialize;

const MAX_TITLE_CHARS: usize = 200;
const MAX_BODY_CHARS: usize = 4000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleSettings {
    /// Five fields, such as "0 7 * * *" for 7:00 every day.
    cron: String,
    /// The title of its events; defaults to the source's name.
    #[serde(default)]
    title: Option<String>,
    /// Text the events carry, such as what to look at.
    #[serde(default)]
    body: String,
}

pub struct ScheduleSource {
    pattern: String,
    cron: Cron,
    title: String,
    body: String,
    trust: Trust,
}

impl ScheduleSource {
    pub fn new(spec: &KindSpec<'_>) -> Result<Self> {
        let settings: ScheduleSettings = options(spec)?;
        let cron = parse_cron(&settings.cron)?;
        let title = settings
            .title
            .unwrap_or_else(|| spec.name.to_string())
            .trim()
            .to_string();
        if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
            bail!("title must be 1 to {MAX_TITLE_CHARS} characters");
        }
        if settings.body.chars().count() > MAX_BODY_CHARS {
            bail!("body must be at most {MAX_BODY_CHARS} characters");
        }
        Ok(Self {
            pattern: settings.cron.trim().to_string(),
            cron,
            title,
            body: settings.body.trim().to_string(),
            // The user wrote it in the config file.
            trust: spec.trust.unwrap_or(Trust::Trusted),
        })
    }
}

/// A five-field cron pattern.
pub fn parse_cron(pattern: &str) -> Result<Cron> {
    CronParser::builder()
        .seconds(Seconds::Disallowed)
        .build()
        .parse(pattern.trim())
        .with_context(|| {
            format!("invalid cron pattern {pattern:?}; use five fields: minute hour day-of-month month day-of-week")
        })
}

/// The first time of `cron` after `after` (Unix seconds), in local time.
pub fn next_after(cron: &Cron, after: u64) -> Option<u64> {
    let start: DateTime<Local> = Local.timestamp_opt(after as i64, 0).single()?;
    let next = cron.find_next_occurrence(&start, false).ok()?;
    u64::try_from(next.timestamp()).ok()
}

/// `at` (Unix seconds) as local time, such as `2026-11-19 07:00`.
pub fn local_time(at: u64) -> String {
    Local
        .timestamp_opt(at as i64, 0)
        .single()
        .map(|time| time.format("%Y-%m-%d %H:%M %:z").to_string())
        .unwrap_or_else(|| at.to_string())
}

impl EventSource for ScheduleSource {
    fn trust(&self) -> Trust {
        self.trust
    }

    fn describe(&self) -> String {
        format!("schedule \"{}\" (server local time)", self.pattern)
    }

    fn receive(&self, _request: &PostedRequest<'_>) -> Result<AutomationEvent, Rejection> {
        Err(Rejection::invalid(
            "this source fires on its schedule; nothing can be posted to it",
        ))
    }

    fn next_fire(&self, after: u64) -> Option<u64> {
        next_after(&self.cron, after)
    }

    fn fired(&self, source: &str, at: u64) -> Option<AutomationEvent> {
        let time = local_time(at);
        let mut body = format!("Scheduled for {time} (\"{}\").", self.pattern);
        if !self.body.is_empty() {
            body = format!("{}\n\n{body}", self.body);
        }
        Some(AutomationEvent {
            source: source.to_string(),
            kind: "schedule".into(),
            key: format!("{source}@{at}"),
            title: self.title.clone(),
            body,
            url: None,
            author: None,
            trust: self.trust,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};
    use serde_json::json;

    fn source(options: serde_json::Value) -> Result<ScheduleSource> {
        ScheduleSource::new(&KindSpec {
            name: "morning",
            trust: None,
            options: &options,
        })
    }

    #[test]
    fn schedules_fire_at_the_next_matching_minute_in_local_time() {
        let schedule = source(json!({"cron": "30 7 * * *", "body": "Plan the day."})).unwrap();
        let now = chrono::Local::now().timestamp() as u64;
        let next = schedule.next_fire(now).unwrap();
        assert!(next > now && next - now <= 24 * 3600 + 3600, "{next} {now}");
        let local = Local.timestamp_opt(next as i64, 0).single().unwrap();
        assert_eq!((local.hour(), local.minute(), local.second()), (7, 30, 0));
        // The time itself does not count again.
        assert!(schedule.next_fire(next).unwrap() > next);

        let event = schedule.fired("morning", next).unwrap();
        assert_eq!(event.key, format!("morning@{next}"));
        assert_eq!(event.title, "morning");
        assert_eq!(event.kind, "schedule");
        assert_eq!(event.trust, Trust::Trusted);
        assert!(
            event.body.starts_with("Plan the day.\n\nScheduled for "),
            "{}",
            event.body
        );
        event.check().unwrap();
    }

    #[test]
    fn weekly_patterns_land_on_their_weekday() {
        let schedule = source(json!({"cron": "0 17 * * FRI", "title": "Weekly review"})).unwrap();
        let next = schedule
            .next_fire(chrono::Local::now().timestamp() as u64)
            .unwrap();
        let local = Local.timestamp_opt(next as i64, 0).single().unwrap();
        assert_eq!(local.weekday(), chrono::Weekday::Fri);
        assert_eq!(schedule.fired("w", next).unwrap().title, "Weekly review");
    }

    #[test]
    fn settings_are_checked_and_nothing_can_be_posted() {
        for broken in [
            json!({"cron": "0 7 * *"}),
            json!({"cron": "0 0 7 * * *"}),
            json!({"cron": "61 7 * * *"}),
            json!({"cron": "0 7 * * *", "title": " "}),
            json!({"cron": "0 7 * * *", "unknown": 1}),
            json!({}),
        ] {
            assert!(source(broken.clone()).is_err(), "{broken}");
        }
        let schedule = source(json!({"cron": "0 7 * * *"})).unwrap();
        let headers = Default::default();
        assert!(schedule
            .receive(&PostedRequest {
                source: "morning",
                headers: &headers,
                body: b"{}",
            })
            .is_err());
        let untrusted = ScheduleSource::new(&KindSpec {
            name: "m",
            trust: Some(Trust::Untrusted),
            options: &json!({"cron": "0 7 * * *"}),
        })
        .unwrap();
        assert_eq!(untrusted.trust(), Trust::Untrusted);
    }
}
