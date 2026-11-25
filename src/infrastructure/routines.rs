//! Routines kept as a `routines.json` file, the `routine_create` and
//! `routine_delete` tools, and the automation source `routines`, through
//! which `ano serve` carries them out at their times.
//!
//! Each user has one file under the platform's data directory (for example
//! `~/Library/Application Support/ano/routines/<user>/routines.json` on
//! macOS).

use crate::{
    application::{
        automation::{EventSource, PostedRequest, Rejection},
        clock::unix_now,
        registry::ToolRegistry,
    },
    domain::{
        automation::{AutomationEvent, Trust},
        routine::{
            check_routine, Routine, RoutineKind, MAX_ROUTINES, ROUTINE_CREATE_NAME,
            ROUTINE_DELETE_NAME,
        },
        tool::ToolDefinition,
    },
    infrastructure::{
        automation::schedule::{local_time, next_after, parse_cron},
        fs::{atomic_write, lock_in, user_directory},
    },
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fs::File, io::ErrorKind, path::PathBuf, sync::Arc};

const ROUTINES_FILE: &str = "routines.json";
const LOCK_FILE: &str = "routines.json.lock";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutineSettings {
    pub enabled: bool,
    /// Where routines are kept. Defaults to `routines` in the platform's
    /// data directory for ano.
    pub dir: Option<PathBuf>,
}

/// Enabled unless the config file turns it off.
impl Default for RoutineSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: None,
        }
    }
}

impl RoutineSettings {
    pub fn directory(&self) -> Result<PathBuf> {
        match &self.dir {
            Some(dir) => Ok(dir.clone()),
            None => default_routines_dir()
                .context("cannot determine the data directory for routines; set routines.dir"),
        }
    }
}

/// `routines` in the platform's data directory: `$XDG_DATA_HOME/ano` (or
/// `~/.local/share/ano`) on Linux, `~/Library/Application Support/ano` on
/// macOS, and `%APPDATA%\ano\data` on Windows.
pub fn default_routines_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ano").map(|dirs| dirs.data_dir().join("routines"))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RoutineFile {
    /// The number of the last routine's id, so ids are never reused.
    #[serde(default)]
    last_id: u64,
    routines: Vec<Routine>,
}

/// The next time of `routine` after `after` (Unix seconds).
pub fn next_time(routine: &Routine, after: u64) -> Option<u64> {
    match (&routine.cron, routine.at) {
        (Some(pattern), _) => next_after(&parse_cron(pattern).ok()?, after),
        (None, Some(at)) => (at > after).then_some(at),
        (None, None) => None,
    }
}

/// A time the user gave, such as `2026-11-20 09:00` (local time) or an
/// RFC 3339 time with its offset, as Unix seconds.
pub fn parse_time(text: &str) -> Result<u64> {
    let text = text.trim();
    let parsed: Option<DateTime<Local>> = DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.with_timezone(&Local))
        .or_else(|| {
            [
                "%Y-%m-%d %H:%M",
                "%Y-%m-%dT%H:%M",
                "%Y-%m-%d %H:%M:%S",
                "%Y-%m-%dT%H:%M:%S",
            ]
            .iter()
            .find_map(|format| NaiveDateTime::parse_from_str(text, format).ok())
            .and_then(|naive| Local.from_local_datetime(&naive).earliest())
        });
    match parsed.and_then(|time| u64::try_from(time.timestamp()).ok()) {
        Some(at) => Ok(at),
        None => bail!("cannot read the time {text:?}; write it like 2026-11-20 09:00 (local time)"),
    }
}

pub struct RoutineBook {
    root: PathBuf,
}

impl RoutineBook {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The book when routines are enabled, with its tools registered.
    pub fn from_settings(
        settings: &RoutineSettings,
        registry: &ToolRegistry,
    ) -> Result<Option<Arc<Self>>> {
        if !settings.enabled {
            return Ok(None);
        }
        let book = Arc::new(Self::new(settings.directory()?));
        book.register_tools(registry)?;
        Ok(Some(book))
    }

    pub fn path(&self, user: &str) -> Result<PathBuf> {
        Ok(user_directory(&self.root, user)?.join(ROUTINES_FILE))
    }

    /// Every routine of `user`, in the order they were set up.
    pub fn list(&self, user: &str) -> Result<Vec<Routine>> {
        Ok(self.load(user)?.routines)
    }

    /// Set up a routine. `cron` repeats it; `at` (Unix seconds) runs it
    /// once.
    pub fn create(
        &self,
        user: &str,
        kind: RoutineKind,
        title: &str,
        task: &str,
        cron: Option<&str>,
        at: Option<u64>,
    ) -> Result<Routine> {
        let now = unix_now();
        let (title, task) = check_routine(title, task, cron, at, now)?;
        let cron = cron.map(|pattern| pattern.trim().to_string());
        if let Some(pattern) = &cron {
            parse_cron(pattern)?;
        }
        let _lock = self.lock(user)?;
        let mut file = self.load(user)?;
        // One-time routines that have passed make room first.
        file.routines
            .retain(|routine| routine.cron.is_some() || routine.at.is_some_and(|at| at > now));
        if file.routines.len() >= MAX_ROUTINES {
            bail!("{MAX_ROUTINES} routines are set up already; remove some first");
        }
        file.last_id += 1;
        let routine = Routine {
            id: format!("r{}", file.last_id),
            kind,
            title,
            task,
            cron,
            at,
            created_at: now,
        };
        file.routines.push(routine.clone());
        self.store(user, &file)?;
        Ok(routine)
    }

    pub fn delete(&self, user: &str, id: &str) -> Result<Routine> {
        let _lock = self.lock(user)?;
        let mut file = self.load(user)?;
        let Some(index) = file.routines.iter().position(|routine| routine.id == id) else {
            let ids: Vec<&str> = file
                .routines
                .iter()
                .map(|routine| routine.id.as_str())
                .collect();
            bail!(
                "no routine {id:?}; known ids: {}",
                if ids.is_empty() {
                    "(none)".into()
                } else {
                    ids.join(", ")
                }
            );
        };
        let removed = file.routines.remove(index);
        self.store(user, &file)?;
        Ok(removed)
    }

    /// The routines of `user` still to come after `now`, soonest first,
    /// with their next times.
    pub fn upcoming(&self, user: &str, now: u64) -> Result<Vec<(u64, Routine)>> {
        let mut upcoming: Vec<(u64, Routine)> = self
            .list(user)?
            .into_iter()
            .filter_map(|routine| Some((next_time(&routine, now)?, routine)))
            .collect();
        upcoming.sort_by_key(|(at, routine)| (*at, routine.id.clone()));
        Ok(upcoming)
    }

    fn load(&self, user: &str) -> Result<RoutineFile> {
        let path = self.path(user)?;
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid routines file {}", path.display())),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(RoutineFile::default()),
            Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    fn store(&self, user: &str, file: &RoutineFile) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(file)?;
        bytes.push(b'\n');
        atomic_write(&self.path(user)?, &bytes, None)
    }

    fn lock(&self, user: &str) -> Result<File> {
        let path = self.path(user)?;
        let directory = path.parent().context("routines file has no directory")?;
        lock_in(directory, LOCK_FILE).with_context(|| {
            format!(
                "routines are being changed by another process: {}",
                path.display()
            )
        })
    }

    pub fn register_tools(self: &Arc<Self>, registry: &ToolRegistry) -> Result<()> {
        if !registry.is_registered(ROUTINE_CREATE_NAME) {
            let book = Arc::clone(self);
            // cron and at are alternatives, which a strict Responses schema
            // cannot express; the handler checks the arguments.
            let mut definition = ToolDefinition::new(
                ROUTINE_CREATE_NAME,
                "Set up a reminder or a recurring task that ano carries out at set times, even when no conversation is open. Requires approval.",
                json!({
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string", "enum": ["reminder", "task"], "description": "reminder: tell the user something at the time. task: do some work at the time and report it."},
                        "title": {"type": "string", "description": "A short name, such as \"Weekly review\"."},
                        "task": {"type": "string", "description": "What to remind the user of, or the work to do and report, in the user's words."},
                        "cron": {"type": "string", "description": "To repeat: five cron fields in local time (minute hour day-of-month month day-of-week), such as \"0 17 * * FRI\"."},
                        "at": {"type": "string", "description": "To happen once: a local time such as \"2026-11-20 09:00\"."}
                    },
                    "required": ["kind", "title", "task"],
                    "additionalProperties": false
                }),
            )
            .with_approval()
            .always_offered();
            definition.strict = false;
            registry.register_contextual(definition, move |arguments, context| {
                let book = Arc::clone(&book);
                async move {
                    tokio::task::spawn_blocking(move || {
                        book.create_tool(&context.user_id, &arguments)
                    })
                    .await
                    .context("routine_create task failed")?
                }
            })?;
        }
        if !registry.is_registered(ROUTINE_DELETE_NAME) {
            let book = Arc::clone(self);
            registry.register_contextual(
                ToolDefinition::new(
                    ROUTINE_DELETE_NAME,
                    "Remove one of the user's reminders or recurring tasks. Requires approval.",
                    json!({
                        "type": "object",
                        "properties": {"id": {"type": "string", "description": "The routine, such as r3."}},
                        "required": ["id"],
                        "additionalProperties": false
                    }),
                )
                .with_approval()
                .always_offered(),
                move |arguments, context| {
                    let book = Arc::clone(&book);
                    async move {
                        tokio::task::spawn_blocking(move || {
                            let id = arguments["id"]
                                .as_str()
                                .context("routine_delete.id must be a string")?;
                            let removed = book.delete(&context.user_id, id.trim())?;
                            Ok(json!({"status": "removed", "routine": removed.title}))
                        })
                        .await
                        .context("routine_delete task failed")?
                    }
                },
            )?;
        }
        Ok(())
    }

    fn create_tool(&self, user: &str, arguments: &Value) -> Result<Value> {
        let field = |key: &str| -> Result<Option<&str>> {
            match &arguments[key] {
                Value::Null => Ok(None),
                Value::String(text) if text.trim().is_empty() => Ok(None),
                Value::String(text) => Ok(Some(text.as_str())),
                _ => bail!("{key} must be a string"),
            }
        };
        let kind = RoutineKind::parse(field("kind")?.context("kind is required")?)?;
        let at = field("at")?.map(parse_time).transpose()?;
        let routine = self.create(
            user,
            kind,
            field("title")?.context("title is required")?,
            field("task")?.context("task is required")?,
            field("cron")?,
            at,
        )?;
        let now = unix_now();
        let next = next_time(&routine, now).map(local_time);
        Ok(json!({
            "status": "created",
            "id": routine.id,
            "next": next,
            "now": local_time(now),
            "note": "It runs while ano serve is running with a routines source.",
        }))
    }
}

/// The automation source `routines`: events for the routines of one user
/// at their times. Nothing can be posted to it.
pub struct RoutineSource {
    book: RoutineBook,
    user: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoutineSourceSettings {
    /// The user whose routines to carry out; `default` when unset.
    #[serde(default)]
    user: Option<String>,
}

impl RoutineSource {
    /// The source for the routines kept under `root`, configured by
    /// `options`.
    pub fn new(root: PathBuf, options: &Value) -> Result<Self> {
        let settings: RoutineSourceSettings =
            serde_json::from_value(options.clone()).context("invalid settings")?;
        let user = settings.user.unwrap_or_else(|| "default".into());
        let book = RoutineBook::new(root);
        book.path(&user)?;
        Ok(Self { book, user })
    }

    fn routines(&self) -> Vec<Routine> {
        match self.book.list(&self.user) {
            Ok(routines) => routines,
            Err(error) => {
                eprintln!(
                    "automation: cannot read the routines of '{}': {error:#}",
                    self.user
                );
                Vec::new()
            }
        }
    }
}

impl EventSource for RoutineSource {
    fn trust(&self) -> Trust {
        // Routines are set up by the user, each with approval.
        Trust::Trusted
    }

    fn describe(&self) -> String {
        format!("routines of user '{}'", self.user)
    }

    fn receive(&self, _request: &PostedRequest<'_>) -> Result<AutomationEvent, Rejection> {
        Err(Rejection::invalid(
            "this source fires at the times of the user's routines; nothing can be posted to it",
        ))
    }

    fn timed(&self) -> bool {
        true
    }

    fn next_fire(&self, after: u64) -> Option<u64> {
        self.routines()
            .iter()
            .filter_map(|routine| next_time(routine, after))
            .min()
    }

    fn fired(&self, source: &str, at: u64) -> Vec<AutomationEvent> {
        self.routines()
            .into_iter()
            .filter(|routine| next_time(routine, at.saturating_sub(1)) == Some(at))
            .map(|routine| {
                let action = match routine.kind {
                    RoutineKind::Reminder => "Remind the user of this now",
                    RoutineKind::Task => "Do this now and tell the user the result",
                };
                let repeat = match &routine.cron {
                    Some(pattern) => format!("repeats \"{pattern}\""),
                    None => "once".into(),
                };
                AutomationEvent {
                    source: source.to_string(),
                    kind: routine.kind.name().into(),
                    key: format!("{}@{at}", routine.id),
                    title: routine.title.clone(),
                    body: format!(
                        "{action} (routine {}, {repeat}, set up by the user; scheduled for {}):\n\n{}",
                        routine.id,
                        local_time(at),
                        routine.task
                    ),
                    url: None,
                    author: None,
                    trust: Trust::Trusted,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routines_are_created_listed_and_removed() {
        let root = tempfile::tempdir().unwrap();
        let book = RoutineBook::new(root.path());
        let created = book
            .create_tool(
                "default",
                &json!({"kind": "task", "title": "Weekly review", "task": "Sum up my week.", "cron": "0 17 * * FRI"}),
            )
            .unwrap();
        assert_eq!(created["id"], "r1");
        assert!(created["next"].as_str().is_some());
        // The current time, for conversations that started long ago.
        assert!(created["now"].as_str().unwrap().starts_with("20"));
        let tomorrow = Local::now() + chrono::Duration::days(1);
        let at = tomorrow.format("%Y-%m-%d %H:%M").to_string();
        book.create_tool(
            "default",
            &json!({"kind": "reminder", "title": "Dentist", "task": "Leave for the dentist.", "at": at}),
        )
        .unwrap();
        for broken in [
            json!({"kind": "alarm", "title": "t", "task": "x", "cron": "* * * * *"}),
            json!({"kind": "task", "title": "t", "task": "x", "cron": "every day"}),
            json!({"kind": "task", "title": "t", "task": "x", "at": "tomorrow"}),
            json!({"kind": "task", "title": "t", "task": "x", "at": "2001-01-01 09:00"}),
            json!({"kind": "task", "title": "t", "task": "x"}),
        ] {
            assert!(book.create_tool("default", &broken).is_err(), "{broken}");
        }
        let upcoming = book.upcoming("default", unix_now()).unwrap();
        assert_eq!(upcoming.len(), 2);
        assert!(upcoming[0].0 <= upcoming[1].0);
        book.delete("default", "r1").unwrap();
        assert!(book.delete("default", "r1").is_err());
        assert_eq!(book.list("default").unwrap().len(), 1);
    }

    #[test]
    fn the_source_fires_each_routine_due_at_a_time() {
        let root = tempfile::tempdir().unwrap();
        let book = RoutineBook::new(root.path());
        let at = unix_now() + 3600;
        let at_text = Local
            .timestamp_opt(at as i64, 0)
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        for title in ["Water the plants", "Call mom"] {
            book.create_tool(
                "alice",
                &json!({"kind": "reminder", "title": title, "task": title, "at": at_text}),
            )
            .unwrap();
        }
        book.create(
            "alice",
            RoutineKind::Task,
            "Later",
            "x",
            None,
            Some(at + 60),
        )
        .unwrap();
        let source = RoutineSource::new(root.path().into(), &json!({"user": "alice"})).unwrap();
        assert_eq!(source.next_fire(unix_now()), Some(at));
        let events = source.fired("routines", at);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].key, format!("r1@{at}"));
        assert_eq!(events[0].kind, "reminder");
        assert!(events[0]
            .body
            .starts_with("Remind the user of this now (routine r1, once,"));
        for event in &events {
            event.check().unwrap();
        }
        assert_eq!(source.next_fire(at), Some(at + 60));
        assert_eq!(source.next_fire(at + 60), None);
        assert!(RoutineSource::new(root.path().into(), &json!({"users": "x"})).is_err());
        assert!(RoutineSource::new(root.path().into(), &json!({"user": "../x"})).is_err());
    }

    #[test]
    fn times_are_read_in_local_time_or_with_an_offset() {
        let local = parse_time("2030-01-02 03:04").unwrap();
        let expected = Local
            .from_local_datetime(
                &NaiveDateTime::parse_from_str("2030-01-02 03:04", "%Y-%m-%d %H:%M").unwrap(),
            )
            .earliest()
            .unwrap()
            .timestamp() as u64;
        assert_eq!(local, expected);
        assert_eq!(parse_time("2030-01-02T03:04:00Z").unwrap(), 1_893_553_440);
        assert!(parse_time("next week").is_err());
    }
}
