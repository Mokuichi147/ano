//! Routines: reminders and recurring tasks the user set up in a
//! conversation ("remind me at 9 tomorrow", "every Friday at 17:00, sum up
//! my week"), which `ano serve` carries out at their times.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// Tool that sets up a routine. It requires approval, because the routine
/// later starts runs on its own.
pub const ROUTINE_CREATE_NAME: &str = "routine_create";
/// Tool that removes a routine.
pub const ROUTINE_DELETE_NAME: &str = "routine_delete";

pub const MAX_TITLE_CHARS: usize = 120;
pub const MAX_TASK_CHARS: usize = 2000;
/// Routines one user may keep, past one-time ones included.
pub const MAX_ROUTINES: usize = 50;
/// Upper bound on the list of upcoming routines sent with one run.
pub const MAX_ROUTINE_INDEX_BYTES: usize = 2 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineKind {
    /// Tell the user something at the time.
    Reminder,
    /// Do some work at the time and report it.
    Task,
}

impl RoutineKind {
    pub fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "reminder" => Self::Reminder,
            "task" => Self::Task,
            _ => bail!("kind must be reminder or task: {text:?}"),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Reminder => "reminder",
            Self::Task => "task",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Routine {
    /// Short and stable, such as `r3`.
    pub id: String,
    pub kind: RoutineKind,
    pub title: String,
    /// What to remind the user of, or the work to do, in the user's words.
    pub task: String,
    /// A five-field cron pattern, for a routine that repeats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    /// Unix seconds, for a routine that happens once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<u64>,
    pub created_at: u64,
}

/// A routine before it gets its id: the fields are checked, and exactly
/// one of `cron` and `at` is set.
pub fn check_routine(
    title: &str,
    task: &str,
    cron: Option<&str>,
    at: Option<u64>,
    now: u64,
) -> Result<(String, String)> {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
        bail!("title must be 1 to {MAX_TITLE_CHARS} characters");
    }
    let task = task.trim().to_string();
    if task.is_empty() || task.chars().count() > MAX_TASK_CHARS {
        bail!("task must be 1 to {MAX_TASK_CHARS} characters");
    }
    if title
        .chars()
        .chain(task.chars())
        .any(|c| c.is_control() && c != '\n')
    {
        bail!("title and task must not contain control characters");
    }
    match (cron, at) {
        (Some(_), Some(_)) => bail!("give either cron (to repeat) or at (once), not both"),
        (None, None) => bail!("give cron (to repeat) or at (once)"),
        (None, Some(at)) if at <= now => bail!("at must be in the future"),
        _ => {}
    }
    Ok((title, task))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routines_need_one_time_rule_and_bounded_text() {
        let (title, task) = check_routine(
            " Weekly \n review ",
            " Sum up the week.\n",
            Some("0 17 * * FRI"),
            None,
            10,
        )
        .unwrap();
        assert_eq!(
            (title.as_str(), task.as_str()),
            ("Weekly review", "Sum up the week.")
        );
        check_routine("Call mom", "Call mom.", None, Some(11), 10).unwrap();
        for (title, task, cron, at) in [
            ("", "x", Some("* * * * *"), None),
            ("t", " ", Some("* * * * *"), None),
            ("t", "x", None, None),
            ("t", "x", Some("* * * * *"), Some(20)),
            ("t", "x", None, Some(10)),
            ("t", "a\u{7}", None, Some(20)),
        ] {
            assert!(
                check_routine(title, task, cron, at, 10).is_err(),
                "{title} {task}"
            );
        }
        assert_eq!(RoutineKind::parse("task").unwrap(), RoutineKind::Task);
        assert!(RoutineKind::parse("alarm").is_err());
    }
}
