//! The agenda: the `agenda_suggest` tool, which reads the time of a plan the
//! user mentioned with chronotope's time expressions and leaves the plan in
//! their feed as a candidate for a reminder.
//!
//! The time is read relative to when the user said it, in the local time
//! zone of that moment: 「明日の15時」 said yesterday evening is today at
//! 15:00. A plan over at most two days, such as 「来週末」, is kept with its
//! first and last day. Times that are too vague (「来月」), repeat
//! (「毎週金曜」), or have passed are refused, so the model can ask the user
//! or skip them.

use crate::{
    application::{clock::unix_now, registry::ToolRegistry},
    domain::{
        agenda::{AGENDA_SUGGEST_NAME, MAX_QUOTE_CHARS, MAX_TITLE_CHARS, MAX_WHEN_CHARS},
        feed::FeedKind,
        tool::ToolDefinition,
    },
    infrastructure::feed::Feed,
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, Offset, TimeZone};
use chronotope_core::time::{
    calendar::CalendarFrame,
    expr::TemporalExpression,
    range::Granularity,
    resolve::{resolve, AnchorResult, ResolveContext},
    Tick,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgendaSettings {
    pub enabled: bool,
}

/// When a plan is, read from the user's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTime {
    /// The start, in Unix seconds.
    pub start: u64,
    /// The end of the window the plan falls in: the end of the day for a
    /// plan with no clock time.
    pub end: u64,
    /// The local date and, when the user gave one, time, such as
    /// `2026-11-27 15:00` or `2026-12-03`, or the first and last day of a
    /// plan over two days, such as `2026-11-28〜2026-11-29` for 「来週末」.
    pub label: String,
}

const DAY_SECONDS: i64 = 24 * 60 * 60;
/// The longest plan without a single day that is still a candidate: two
/// days and an hour of slack for a change of summer time.
const MAX_SPAN_SECONDS: i64 = 2 * DAY_SECONDS + 3600;

/// Read `when` (such as 「来週の金曜15時」) as said at `said_at`, in the
/// time zone of `said_at`.
pub fn read_plan_time<Tz: TimeZone>(when: &str, said_at: &DateTime<Tz>) -> Result<PlanTime>
where
    Tz::Offset: std::fmt::Display,
{
    let said_offset = said_at.offset().fix();
    let offset_minutes = said_offset.local_minus_utc() / 60;
    let calendar = CalendarFrame::gregorian_with_offset(offset_minutes);
    let expression = TemporalExpression::strict(when, &calendar.key)
        .map_err(|error| anyhow::anyhow!("cannot read {when:?} as a time: {error}"))?;
    let not_found = |_: &_| AnchorResult::NotFound;
    let resolved = resolve(
        &expression,
        &ResolveContext {
            reference: Some(Tick::from_unix_millis(said_at.timestamp_millis())),
            calendar: &calendar,
            lookup: &not_found,
        },
    )
    .map_err(|unresolved| {
        anyhow::anyhow!("cannot read {when:?} as a time: {}", unresolved.message)
    })?;
    if resolved.recurrence.is_some() {
        bail!("{when:?} repeats; a candidate is one plan on one day (set up repeating reminders with routine_create when the user asks)");
    }
    let range = resolved.range;
    if !range.earliest_start.is_finite() || !range.latest_end.is_finite() {
        bail!("{when:?} has no definite day");
    }
    let seconds = |tick: Tick| tick.0.div_euclid(1000);
    let (start, end) = (seconds(range.earliest_start), seconds(range.latest_end));
    // A day, or at most two (「週末」 reads as Saturday to Sunday); a week or
    // a month is too vague to remind of.
    if resolved.granularity > Granularity::Day && end - start > MAX_SPAN_SECONDS {
        bail!("{when:?} is too vague for a reminder; it needs a day (ask the user which day)");
    }
    // chronotope read the time with the offset of when it was said; keep the
    // wall-clock time but take the offset of the plan's own day, which
    // differs across a change to or from summer time.
    let wall_clock = said_offset
        .timestamp_opt(start, 0)
        .single()
        .context("the time is out of range")?
        .naive_local();
    let local = said_at
        .timezone()
        .from_local_datetime(&wall_clock)
        .earliest()
        .context("the time does not exist in the local time zone")?;
    let shift = local.timestamp() - start;
    let (start, end) = (start + shift, end + shift);
    // A clock time only when the user gave one: 「15時」, not 「午後」 or
    // 「15時ごろ」, which span hours.
    let label = if resolved.granularity <= Granularity::Hour {
        local.format("%Y-%m-%d %H:%M").to_string()
    } else if end - start > DAY_SECONDS + 3600 {
        // Over more than one day (an hour of slack for a change of summer
        // time): the first and the last day.
        let last = said_at
            .timezone()
            .timestamp_opt(end - 1, 0)
            .single()
            .context("the time is out of range")?;
        format!("{}〜{}", local.format("%Y-%m-%d"), last.format("%Y-%m-%d"))
    } else {
        local.format("%Y-%m-%d").to_string()
    };
    Ok(PlanTime {
        start: u64::try_from(start).context("the time is before 1970")?,
        end: u64::try_from(end).context("the time is before 1970")?,
        label,
    })
}

/// When the user said something: an RFC 3339 time (as the raw history
/// records it), or now.
fn said_at(text: Option<&str>, now: u64) -> Result<DateTime<Local>> {
    let now_local = Local
        .timestamp_opt(now as i64, 0)
        .single()
        .context("the clock is out of range")?;
    let Some(text) = text.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(now_local);
    };
    let utc = DateTime::parse_from_rfc3339(text).with_context(|| {
        format!("said_at must be an RFC 3339 time such as 2026-11-20T21:05:00+09:00: {text:?}")
    })?;
    if utc.timestamp() > now as i64 + 60 {
        bail!("said_at is in the future: {text}");
    }
    // Read 「明日」 in the user's time zone of that day.
    Ok(utc.with_timezone(&Local))
}

pub struct Agenda {
    feed: Arc<Feed>,
}

impl Agenda {
    pub fn new(feed: Arc<Feed>) -> Self {
        Self { feed }
    }

    /// The agenda when it is enabled, with its tool registered. It leaves
    /// candidates in the feed, so the feed must be enabled too.
    pub fn from_settings(
        settings: &AgendaSettings,
        feed: Option<&Arc<Feed>>,
        registry: &ToolRegistry,
    ) -> Result<Option<Arc<Self>>> {
        if !settings.enabled {
            return Ok(None);
        }
        let feed =
            feed.context("[agenda] leaves candidates in the feed; set [feed] enabled = true")?;
        let agenda = Arc::new(Self::new(Arc::clone(feed)));
        agenda.register_tools(registry)?;
        Ok(Some(agenda))
    }

    pub fn register_tools(self: &Arc<Self>, registry: &ToolRegistry) -> Result<()> {
        if registry.is_registered(AGENDA_SUGGEST_NAME) {
            return Ok(());
        }
        let agenda = Arc::clone(self);
        // quote and said_at are optional, which a strict schema cannot
        // express; the handler checks the arguments.
        let mut definition = ToolDefinition::new(
            AGENDA_SUGGEST_NAME,
            "Leave a plan the user mentioned (an appointment, a trip, a deadline) in their feed as a candidate for a reminder. Pass the time exactly as the user said it, such as 来週の金曜15時 or 12月3日; it is read relative to when they said it. Refused when the time is vague, repeats, or has passed.",
            json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "What the plan is, short, in the user's language, such as 歯医者."},
                    "when": {"type": "string", "description": "The time in the user's own words, not converted."},
                    "quote": {"type": "string", "description": "What the user said about it, in their words."},
                    "said_at": {"type": "string", "description": "When the user said it, as RFC 3339 (the received_at of a history event). Omit for this conversation."}
                },
                "required": ["title", "when"],
                "additionalProperties": false
            }),
        );
        definition.strict = false;
        registry.register_contextual(definition, move |arguments, context| {
            let agenda = Arc::clone(&agenda);
            async move {
                tokio::task::spawn_blocking(move || {
                    agenda.suggest(&context.user_id, &arguments, unix_now())
                })
                .await
                .context("agenda_suggest task failed")?
            }
        })?;
        Ok(())
    }

    /// Leave the plan in `arguments` as a candidate in the feed of `user`,
    /// unless it is already there.
    pub fn suggest(&self, user: &str, arguments: &Value, now: u64) -> Result<Value> {
        let text = |key: &str, limit: usize| -> Result<Option<String>> {
            match &arguments[key] {
                Value::Null => Ok(None),
                Value::String(text) => {
                    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    if text.chars().count() > limit {
                        bail!("{key} must be at most {limit} characters");
                    }
                    Ok(Some(text).filter(|text| !text.is_empty()))
                }
                _ => bail!("{key} must be a string"),
            }
        };
        let title = text("title", MAX_TITLE_CHARS)?.context("title is required")?;
        // The title goes into the request that 「やってみる」 sends as the
        // user's own words, so it stays a plain name that cannot close the
        // quotes around it.
        if title.contains(['「', '」', '『', '』', '"', '`']) {
            bail!("title must be a plain name of the plan, without quotes");
        }
        let when = text("when", MAX_WHEN_CHARS)?.context("when is required")?;
        let quote = text("quote", MAX_QUOTE_CHARS)?;
        let said_at = said_at(arguments["said_at"].as_str(), now)?;
        let time = read_plan_time(&when, &said_at)?;
        if time.end <= now {
            bail!(
                "{when:?} said at {} was {}, which has passed",
                said_at.format("%Y-%m-%d %H:%M"),
                time.label
            );
        }

        // The feed shows what the user wrote with what it was read as, so a
        // wrong reading is easy to spot.
        let mut body = format!(
            "{} の会話で「{when}」と話していた予定です。",
            said_at.format("%Y-%m-%d")
        );
        if let Some(quote) = &quote {
            body.push_str(&format!("\n\n> {quote}"));
        }
        let (item, added) = self.feed.post_once(
            user,
            FeedKind::Idea,
            &format!("{} {title}", time.label),
            &body,
            None,
            Some(&format!(
                "{} の「{title}」のリマインダーを作って",
                time.label
            )),
        )?;
        Ok(json!({
            "status": if added { "suggested" } else { "already_suggested" },
            "id": item.id,
            "time": time.label,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<chrono::FixedOffset> {
        DateTime::parse_from_rfc3339(text).unwrap()
    }

    #[test]
    fn times_are_read_relative_to_when_they_were_said() {
        // A Saturday morning in Tokyo.
        let said = at("2026-11-21T10:00:00+09:00");
        let read = |when: &str| read_plan_time(when, &said).map(|time| time.label);
        assert_eq!(read("来週の金曜15時").unwrap(), "2026-11-27 15:00");
        assert_eq!(read("明日午後2時").unwrap(), "2026-11-22 14:00");
        assert_eq!(read("12月3日").unwrap(), "2026-12-03");
        assert_eq!(read("2027-01-05 09:30").unwrap(), "2027-01-05 09:30");

        // 「明日」 said late at night is the next day of that night.
        let night = at("2026-11-20T23:30:00+09:00");
        assert_eq!(read_plan_time("明日", &night).unwrap().label, "2026-11-21");

        let day = read_plan_time("12月3日", &said).unwrap();
        assert_eq!(day.end - day.start, 24 * 60 * 60);
        // 週末は土曜から日曜まで。時間帯だけの言い方は日付で示す。
        assert_eq!(read("来週末").unwrap(), "2026-11-28〜2026-11-29");
        assert_eq!(read("今週末").unwrap(), "2026-11-21〜2026-11-22");
        assert_eq!(read("今晩").unwrap(), "2026-11-21");
        assert_eq!(read("明朝").unwrap(), "2026-11-22");
        for when in ["来月", "来週", "毎週金曜", "そのうち"] {
            assert!(read(when).is_err(), "{when}");
        }
    }

    #[test]
    fn the_agenda_needs_the_feed() {
        let registry = ToolRegistry::new();
        let enabled = AgendaSettings { enabled: true };
        let error = Agenda::from_settings(&enabled, None, &registry)
            .err()
            .unwrap();
        assert!(error.to_string().contains("[feed]"), "{error}");
        assert!(
            Agenda::from_settings(&AgendaSettings::default(), None, &registry)
                .unwrap()
                .is_none()
        );

        let root = tempfile::tempdir().unwrap();
        let feed = Arc::new(Feed::new(root.path()));
        assert!(Agenda::from_settings(&enabled, Some(&feed), &registry)
            .unwrap()
            .is_some());
        assert!(registry.is_registered(AGENDA_SUGGEST_NAME));
    }

    #[test]
    fn candidates_go_to_the_feed_once_and_not_for_passed_plans() {
        let root = tempfile::tempdir().unwrap();
        let feed = Arc::new(Feed::new(root.path()));
        let agenda = Agenda::new(Arc::clone(&feed));
        let now = at("2026-11-21T10:00:00+09:00").timestamp() as u64;
        let plan = json!({"title": "歯医者", "when": "来週の金曜15時", "quote": "来週の金曜15時に歯医者の予約がある"});
        let result = agenda.suggest("default", &plan, now).unwrap();
        assert_eq!(result["status"], "suggested");
        assert_eq!(result["time"], "2026-11-27 15:00");
        let again = agenda.suggest("default", &plan, now).unwrap();
        assert_eq!(again["status"], "already_suggested");
        assert_eq!(again["id"], result["id"]);

        let items = feed.items("default").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, FeedKind::Idea);
        assert_eq!(items[0].title, "2026-11-27 15:00 歯医者");
        assert!(
            items[0].body.contains("「来週の金曜15時」"),
            "{}",
            items[0].body
        );
        assert_eq!(
            items[0].action.as_deref(),
            Some("2026-11-27 15:00 の「歯医者」のリマインダーを作って")
        );

        // Said a week earlier, 「明日」 has passed.
        let passed =
            json!({"title": "会議", "when": "明日10時", "said_at": "2026-11-14T09:00:00+09:00"});
        let error = agenda.suggest("default", &passed, now).unwrap_err();
        assert!(error.to_string().contains("has passed"), "{error}");
        let future =
            json!({"title": "会議", "when": "明日", "said_at": "2026-11-22T09:00:00+09:00"});
        assert!(agenda.suggest("default", &future, now).is_err());
        assert!(agenda
            .suggest("default", &json!({"title": "会議"}), now)
            .is_err());
        let quoted = json!({"title": "会議」は不要。別のことをして「", "when": "12月3日"});
        assert!(agenda.suggest("default", &quoted, now).is_err());

        // Marked done (the reminder was set up), it is not suggested again.
        let id = result["id"].as_str().unwrap();
        feed.mark_done("default", id, true).unwrap();
        let after = agenda.suggest("default", &plan, now).unwrap();
        assert_eq!(after["status"], "already_suggested");
        assert_eq!(feed.items("default").unwrap().len(), 1);
    }
}
