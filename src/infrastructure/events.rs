//! Events from articles: the `event_save` and `event_search` tools.
//!
//! The model reads an article (one the user shared or keeps to read later),
//! and saves each dated event it announces: its name, the time as the
//! article wrote it (such as 「11月28日から29日まで」), the place, and the
//! passage it comes from. chronotope keeps the event as public knowledge with
//! the article as its source, and reads the time relative to when the
//! article was published. ano keeps only which events each user saved, so a
//! search shows that user's events and never the user's identity in the
//! public data.
//!
//! `event_search` finds them by period in the user's own words (「来週末」),
//! which chronotope resolves in the local calendar.

use crate::{
    application::{clock::unix_now, registry::ToolRegistry},
    domain::{
        event::{
            EVENT_SAVE_NAME, EVENT_SEARCH_NAME, MAX_NAME_CHARS, MAX_PASSAGE_CHARS, MAX_PLACE_CHARS,
            MAX_URL_CHARS, MAX_WHEN_CHARS,
        },
        tool::ToolDefinition,
    },
    infrastructure::{
        chronotope::{local_calendar, Chronotope},
        fs::{atomic_write, lock_in, user_directory},
    },
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, NaiveDate, Offset, TimeZone};
use chronotope_core::time::{
    calendar::CalendarFrame,
    expr::TemporalExpression,
    range::Granularity,
    resolve::{resolve, AnchorResult, ResolveContext},
    Tick,
};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, fs::File, io::ErrorKind, path::PathBuf, sync::Arc};

const EVENTS_FILE: &str = "events.json";
const LOCK_FILE: &str = "events.json.lock";
/// Saved events kept per user; the oldest beyond it are forgotten.
const MAX_SAVED: usize = 1000;
/// Hits asked of chronotope's search before keeping the user's own.
const SEARCH_LIMIT: usize = 500;
/// Events `event_search` returns at most.
const MAX_RESULTS: usize = 30;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EventSettings {
    pub enabled: bool,
    /// Where the saved events of each user are listed. Defaults to `events`
    /// in the platform's data directory for ano.
    pub dir: Option<PathBuf>,
}

impl EventSettings {
    pub fn directory(&self) -> Result<PathBuf> {
        match &self.dir {
            Some(dir) => Ok(dir.clone()),
            None => default_events_dir()
                .context("cannot determine the data directory for events; set events.dir"),
        }
    }
}

/// `events` in the platform's data directory for ano.
pub fn default_events_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ano").map(|dirs| dirs.data_dir().join("events"))
}

/// An event a user saved: the chronotope resource, with what is needed to
/// show it without asking chronotope (and to find it again).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedEvent {
    /// The event's resource in chronotope.
    pub resource: String,
    pub name: String,
    /// The time as the article wrote it.
    pub when: String,
    /// The local dates (and time), such as `2026-11-28〜2026-11-29`.
    pub time: String,
    /// Unix seconds of the start and end of the time.
    pub start: i64,
    pub end: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<String>,
    pub url: String,
    pub saved_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct EventsFile {
    events: Vec<SavedEvent>,
}

/// A time read from an article: its range and how to show it.
#[derive(Debug, Clone, PartialEq)]
pub struct EventTime {
    pub start: i64,
    pub end: i64,
    pub label: String,
}

/// Read `when` (such as 「11月28日から29日まで」) as written in an article
/// published at `published`, in that moment's local time zone. Refused when
/// it cannot be read, repeats, or has no definite start and end. Dates are
/// shown in the time zone of `published`.
pub fn read_event_time<Tz: TimeZone>(when: &str, published: &DateTime<Tz>) -> Result<EventTime>
where
    Tz::Offset: std::fmt::Display,
{
    let offset_minutes = published.offset().fix().local_minus_utc() / 60;
    let calendar = CalendarFrame::gregorian_with_offset(offset_minutes);
    let expression = TemporalExpression::strict(when, &calendar.key)
        .map_err(|error| anyhow::anyhow!("cannot read {when:?} as a time: {error}"))?;
    let not_found = |_: &_| AnchorResult::NotFound;
    let resolved = resolve(
        &expression,
        &ResolveContext {
            reference: Some(Tick::from_unix_millis(published.timestamp_millis())),
            calendar: &calendar,
            lookup: &not_found,
        },
    )
    .map_err(|unresolved| {
        anyhow::anyhow!("cannot read {when:?} as a time: {}", unresolved.message)
    })?;
    if resolved.recurrence.is_some() {
        bail!("{when:?} repeats; save one event with a definite time");
    }
    let range = resolved.range;
    if !range.earliest_start.is_finite() || !range.latest_end.is_finite() {
        bail!("{when:?} has no definite start and end");
    }
    let seconds = |tick: Tick| tick.0.div_euclid(1000);
    let (start, end) = (seconds(range.earliest_start), seconds(range.latest_end));
    let local = |at: i64| {
        published
            .timezone()
            .timestamp_opt(at, 0)
            .single()
            .context("the time is out of range")
    };
    let first = local(start)?;
    let last = local(end - 1)?;
    let label = if resolved.granularity <= Granularity::Hour {
        first.format("%Y-%m-%d %H:%M").to_string()
    } else if first.date_naive() == last.date_naive() {
        first.format("%Y-%m-%d").to_string()
    } else {
        format!("{}〜{}", first.format("%Y-%m-%d"), last.format("%Y-%m-%d"))
    };
    Ok(EventTime { start, end, label })
}

/// When the article was published: an RFC 3339 time or a date, read in the
/// local time zone; now when omitted. Never later than now.
fn published_at(text: Option<&str>, now: u64) -> Result<DateTime<Local>> {
    let now_local = Local
        .timestamp_opt(now as i64, 0)
        .single()
        .context("the clock is out of range")?;
    let Some(text) = text.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(now_local);
    };
    let at = match DateTime::parse_from_rfc3339(text) {
        Ok(at) => at.with_timezone(&Local),
        Err(_) => {
            let date = NaiveDate::parse_from_str(text, "%Y-%m-%d").with_context(|| {
                format!("published must be a date such as 2026-11-20 or an RFC 3339 time: {text:?}")
            })?;
            date.and_hms_opt(0, 0, 0)
                .and_then(|midnight| Local.from_local_datetime(&midnight).earliest())
                .context("the publication date does not exist in the local time zone")?
        }
    };
    if at.timestamp() > now as i64 + 60 {
        bail!("published is in the future: {text}");
    }
    Ok(at)
}

/// A plain string argument, whitespace collapsed, within `limit` characters.
fn text(arguments: &Value, key: &str, limit: usize) -> Result<Option<String>> {
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
}

/// An http(s) URL without credentials: where the article is.
fn article_url(arguments: &Value) -> Result<String> {
    let url = text(arguments, "url", MAX_URL_CHARS)?.context("url is required")?;
    let parsed = Url::parse(&url).with_context(|| format!("url is not a URL: {url:?}"))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        bail!("url must be the http(s) address of the article, without credentials");
    }
    Ok(url)
}

pub struct Events {
    root: PathBuf,
    chronotope: Arc<Chronotope>,
}

impl Events {
    pub fn new(root: impl Into<PathBuf>, chronotope: Arc<Chronotope>) -> Self {
        Self {
            root: root.into(),
            chronotope,
        }
    }

    /// Events when they are enabled, with their tools registered. They are
    /// kept in chronotope, so the raw history's connection must be set up.
    pub fn from_settings(
        settings: &EventSettings,
        chronotope: Option<&Arc<Chronotope>>,
        registry: &ToolRegistry,
    ) -> Result<Option<Arc<Self>>> {
        if !settings.enabled {
            return Ok(None);
        }
        let chronotope = chronotope
            .context("[events] keeps events in chronotope; set [history] enabled = true")?;
        let events = Arc::new(Self::new(settings.directory()?, Arc::clone(chronotope)));
        events.register_tools(registry)?;
        Ok(Some(events))
    }

    pub fn register_tools(self: &Arc<Self>, registry: &ToolRegistry) -> Result<()> {
        if !registry.is_registered(EVENT_SAVE_NAME) {
            let events = Arc::clone(self);
            // place and published are optional, which a strict schema cannot
            // express; the handler checks the arguments.
            let mut definition = ToolDefinition::new(
                EVENT_SAVE_NAME,
                "Save one dated event that an article announces (a festival, an exhibition, a sale, a release), so it can be found by period later. Call it once per event. Pass the time exactly as the article wrote it, such as 11月28日から29日まで; it is read relative to when the article was published. Saving the same event of the same article again returns the saved one.",
                json!({
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "The http(s) address of the article."},
                        "name": {"type": "string", "description": "The event's name as the article gives it, such as 代々木クラフトビールフェス2026."},
                        "when": {"type": "string", "description": "The time as the article wrote it, not converted."},
                        "place": {"type": "string", "description": "Where it takes place, as the article wrote it."},
                        "passage": {"type": "string", "description": "The sentences of the article that announce the event, copied as written."},
                        "published": {"type": "string", "description": "When the article was published, as a date (2026-11-20) or an RFC 3339 time. Omit when unknown."}
                    },
                    "required": ["url", "name", "when", "passage"],
                    "additionalProperties": false
                }),
            );
            definition.strict = false;
            registry.register_contextual(definition, move |arguments, context| {
                let events = Arc::clone(&events);
                async move { events.save(&context.user_id, &arguments, unix_now()).await }
            })?;
        }
        if !registry.is_registered(EVENT_SEARCH_NAME) {
            let events = Arc::clone(self);
            let mut definition = ToolDefinition::new(
                EVENT_SEARCH_NAME,
                "Find the events the user saved from articles. Give the period as time in the user's words (来週末, 今月, 12月), not as dates you computed, and text to match names or places. With neither, it lists the saved events that have not ended yet.",
                json!({
                    "type": "object",
                    "properties": {
                        "time": {"type": "string", "description": "The period in the user's words, such as 来週末 or 12月."},
                        "text": {"type": "string", "description": "Words to look for in the event's name or place."}
                    },
                    "additionalProperties": false
                }),
            );
            definition.strict = false;
            registry.register_contextual(definition, move |arguments, context| {
                let events = Arc::clone(&events);
                async move {
                    events
                        .search(&context.user_id, &arguments, unix_now())
                        .await
                }
            })?;
        }
        Ok(())
    }

    /// Save the event in `arguments` for `user`, as of `now`.
    pub async fn save(&self, user: &str, arguments: &Value, now: u64) -> Result<Value> {
        let url = article_url(arguments)?;
        let name = text(arguments, "name", MAX_NAME_CHARS)?.context("name is required")?;
        // The name is shown to the user and may go into requests; keep it a
        // plain name that cannot close quotes around it.
        if name.contains(['「', '」', '『', '』', '"', '`']) {
            bail!("name must be the plain name of the event, without quotes");
        }
        let when = text(arguments, "when", MAX_WHEN_CHARS)?.context("when is required")?;
        let place = text(arguments, "place", MAX_PLACE_CHARS)?;
        let passage = match &arguments["passage"] {
            Value::String(passage) if !passage.trim().is_empty() => passage.trim().to_string(),
            _ => bail!("passage is required: the sentences of the article about the event"),
        };
        if passage.chars().count() > MAX_PASSAGE_CHARS {
            bail!("passage must be at most {MAX_PASSAGE_CHARS} characters");
        }
        let published_text = text(arguments, "published", 40)?;
        let published = published_at(published_text.as_deref(), now)?;
        let time = read_event_time(&when, &published)?;

        if let Some(saved) = self
            .load(user)?
            .events
            .into_iter()
            .find(|event| event.url == url && event.name == name)
        {
            return Ok(json!({"status": "already_saved", "name": saved.name, "time": saved.time}));
        }

        let calendar = local_calendar();
        let mut entities = vec![json!({"ref": "E1", "types": ["Event"], "label": name})];
        let mut claims = vec![json!({
            "subject": "E1", "predicate": "occurred_at",
            "object": {"time": when, "calendar": calendar},
        })];
        if let Some(place) = &place {
            entities
                .push(json!({"ref": "P1", "types": ["Place"], "label": place, "mention": place}));
            claims.push(
                json!({"subject": "E1", "predicate": "took_place_at", "object": {"ref": "P1"}}),
            );
        }
        let body = json!({
            "document": {
                "text": passage,
                "url": url,
                "published": published.to_rfc3339(),
                "calendar": calendar,
            },
            "extraction": {
                "extractor": {"name": "ano"},
                "source_time": published.to_rfc3339(),
                "entities": entities,
                "claims": claims,
            },
            "dry_run": false,
        });
        let reply = self.chronotope.request(user, "ingest/text", &body).await?;
        let resource = reply["entities"]
            .as_array()
            .and_then(|entities| entities.iter().find(|entity| entity["ref"] == "E1"))
            .and_then(|entity| entity["resource"].as_str())
            .context("chronotope did not return the event it saved")?
            .to_string();
        // Make the event searchable now rather than when the materializer
        // gets to it.
        let materialized = self
            .chronotope
            .request(user, "materialize", &json!({}))
            .await;

        let saved = SavedEvent {
            resource,
            name,
            when,
            time: time.label.clone(),
            start: time.start,
            end: time.end,
            place,
            url,
            saved_at: now,
        };
        {
            let _lock = self.lock(user)?;
            let mut file = self.load(user)?;
            if !file
                .events
                .iter()
                .any(|event| event.url == saved.url && event.name == saved.name)
            {
                file.events.push(saved.clone());
                if file.events.len() > MAX_SAVED {
                    let excess = file.events.len() - MAX_SAVED;
                    file.events.drain(..excess);
                }
                self.store(user, &file)?;
            }
        }
        let mut result = json!({"status": "saved", "name": saved.name, "time": saved.time});
        if materialized.is_err() {
            result["warning"] =
                json!("chronotope has not indexed it yet; a search right away may miss it");
        }
        Ok(result)
    }

    /// Find `user`'s saved events by the period and text in `arguments`.
    pub async fn search(&self, user: &str, arguments: &Value, now: u64) -> Result<Value> {
        let time = text(arguments, "time", MAX_WHEN_CHARS)?;
        let words = text(arguments, "text", MAX_NAME_CHARS)?;
        let saved = self.load(user)?.events;
        if time.is_none() && words.is_none() {
            let mut upcoming: Vec<&SavedEvent> = saved
                .iter()
                .filter(|event| event.end > now as i64)
                .collect();
            upcoming.sort_by_key(|event| event.start);
            let more = upcoming.len() > MAX_RESULTS;
            let events: Vec<Value> = upcoming
                .into_iter()
                .take(MAX_RESULTS)
                .map(|event| {
                    json!({"name": event.name, "time": event.time, "when": event.when, "place": event.place, "url": event.url})
                })
                .collect();
            return Ok(json!({"events": events, "has_more": more}));
        }
        if saved.is_empty() {
            return Ok(json!({"events": [], "note": "No events have been saved yet."}));
        }
        let by_resource: HashMap<&str, &SavedEvent> = saved
            .iter()
            .map(|event| (event.resource.as_str(), event))
            .collect();

        let mut query = json!({
            "op": "search",
            "budget_ms": 1000,
            "types": ["Event"],
            "status": ["proposed", "accepted"],
            "limit": SEARCH_LIMIT,
        });
        if let Some(time) = &time {
            query["time"] = json!({"expression": time, "calendar": local_calendar()});
        }
        if let Some(words) = &words {
            query["text"] = json!(words);
        }
        let mut reply = self.chronotope.request(user, "query", &query).await?;
        let mut warnings = Vec::new();
        // Recently saved events reach the search once materialized.
        if reply["freshness"]["consistent"] == false {
            if self
                .chronotope
                .request(user, "materialize", &json!({}))
                .await
                .is_ok()
            {
                reply = self.chronotope.request(user, "query", &query).await?;
            }
            if reply["freshness"]["consistent"] == false {
                warnings.push("chronotope has not indexed every event yet; recently saved ones may be missing");
            }
        }
        let hits = reply["results"].as_array().cloned().unwrap_or_default();
        if reply["truncated"] == true || hits.len() >= SEARCH_LIMIT {
            warnings.push("the search stopped early; some saved events may be missing");
        }
        let mut events = Vec::new();
        for hit in &hits {
            let Some(event) = hit["id"].as_str().and_then(|id| by_resource.get(id)) else {
                continue;
            };
            events.push(json!({
                "name": event.name,
                "time": event.time,
                "when": event.when,
                "place": event.place,
                "url": event.url,
            }));
            if events.len() >= MAX_RESULTS {
                warnings.push("more events match; narrow the period or the text");
                break;
            }
        }
        let mut result = json!({"events": events});
        if let Some(time) = time {
            result["period"] = json!(time);
        }
        if !warnings.is_empty() {
            result["warnings"] = json!(warnings);
        }
        Ok(result)
    }

    /// The events `user` saved, oldest first.
    pub fn saved(&self, user: &str) -> Result<Vec<SavedEvent>> {
        Ok(self.load(user)?.events)
    }

    fn path(&self, user: &str) -> Result<PathBuf> {
        Ok(user_directory(&self.root, user)?.join(EVENTS_FILE))
    }

    fn load(&self, user: &str) -> Result<EventsFile> {
        let path = self.path(user)?;
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid events file {}", path.display())),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(EventsFile::default()),
            Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    fn store(&self, user: &str, file: &EventsFile) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(file)?;
        bytes.push(b'\n');
        atomic_write(&self.path(user)?, &bytes, None)
    }

    fn lock(&self, user: &str) -> Result<File> {
        let path = self.path(user)?;
        let directory = path.parent().context("events file has no directory")?;
        lock_in(directory, LOCK_FILE).with_context(|| {
            format!(
                "the events are being changed by another process: {}",
                path.display()
            )
        })
    }
}

#[cfg(test)]
mod tests;
