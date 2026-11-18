//! Objectives: what the user is working toward over days or months, such as
//! a habit, a savings target, or something to learn, and the progress
//! recorded toward each.
//!
//! An objective is not the goal of one run (`TaskGoal`, `--goal`): it
//! outlives conversations, and runs read the active ones so they can help
//! the user move forward and record progress.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// Tool that creates an objective or changes one. It requires approval,
/// because objectives are listed to every later run.
pub const OBJECTIVE_SET_NAME: &str = "objective_set";
/// Tool that records progress toward an objective.
pub const OBJECTIVE_LOG_NAME: &str = "objective_log";
/// Tool that reads an objective with its whole progress log.
pub const OBJECTIVE_READ_NAME: &str = "objective_read";

pub const MAX_TITLE_CHARS: usize = 120;
pub const MAX_TEXT_CHARS: usize = 500;
pub const MAX_UNIT_CHARS: usize = 20;
/// Objectives one user may keep, finished ones included.
pub const MAX_OBJECTIVES: usize = 100;
/// Progress entries kept per objective; the oldest are dropped beyond it.
pub const MAX_LOG_ENTRIES: usize = 200;
/// Upper bound on the list of active objectives sent with one run.
pub const MAX_OBJECTIVE_INDEX_BYTES: usize = 3 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Active,
    Paused,
    Done,
    Dropped,
}

impl Status {
    pub fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "active" => Self::Active,
            "paused" => Self::Paused,
            "done" => Self::Done,
            "dropped" => Self::Dropped,
            _ => bail!("status must be active, paused, done, or dropped: {text:?}"),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Done => "done",
            Self::Dropped => "dropped",
        }
    }
}

/// A number to reach, such as 100000 yen or 30 days.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Target {
    pub value: f64,
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    /// Unix seconds.
    pub at: u64,
    pub note: String,
    /// The measured value now (not an increment), when the objective has a
    /// target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Objective {
    /// Short and stable, such as `o3`.
    pub id: String,
    pub title: String,
    /// A free word that groups objectives, such as health or money.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub category: String,
    /// Why the user wants it; helps keep suggestions on point.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
    #[serde(default)]
    pub status: Status,
    /// The next concrete action.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub next_step: String,
    /// A date the user aims for, as written (for example 2026-12-31).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub due: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Target>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub log: Vec<Progress>,
}

impl Objective {
    /// The latest measured value, if any progress carried one.
    pub fn current_value(&self) -> Option<f64> {
        self.log.iter().rev().find_map(|entry| entry.value)
    }

    /// One line for the run's instructions.
    pub fn summary(&self) -> String {
        let mut line = format!("[{}] {}", self.id, self.title);
        if !self.category.is_empty() {
            line.push_str(&format!(" ({})", self.category));
        }
        if let Some(target) = &self.target {
            match self.current_value() {
                Some(value) => line.push_str(&format!(
                    "; {} / {} {}",
                    format_number(value),
                    format_number(target.value),
                    target.unit
                )),
                None => line.push_str(&format!(
                    "; target {} {}",
                    format_number(target.value),
                    target.unit
                )),
            }
        }
        if !self.due.is_empty() {
            line.push_str(&format!("; due {}", self.due));
        }
        if !self.next_step.is_empty() {
            line.push_str(&format!("; next: {}", self.next_step));
        }
        if let Some(last) = self.log.last() {
            line.push_str(&format!("; last progress: {}", last.note));
        }
        line
    }
}

pub fn format_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// A one-line field within `limit` characters, with whitespace collapsed.
/// Empty is allowed; `what` names the field in errors.
pub fn one_line(text: &str, limit: usize, what: &str) -> Result<String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > limit {
        bail!("{what} must be at most {limit} characters");
    }
    if text.chars().any(char::is_control) {
        bail!("{what} must not contain control characters");
    }
    Ok(text)
}

/// Changes to an objective: given fields replace the current ones; an empty
/// string clears an optional text field.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ObjectiveChange {
    pub title: Option<String>,
    pub category: Option<String>,
    pub why: Option<String>,
    pub status: Option<Status>,
    pub next_step: Option<String>,
    pub due: Option<String>,
    /// `Some(None)` removes the target.
    pub target: Option<Option<Target>>,
}

impl ObjectiveChange {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Apply the change to `objective` at `now`.
    pub fn apply(&self, objective: &mut Objective, now: u64) -> Result<()> {
        if let Some(title) = &self.title {
            let title = one_line(title, MAX_TITLE_CHARS, "title")?;
            if title.is_empty() {
                bail!("title must not be empty");
            }
            objective.title = title;
        }
        if let Some(category) = &self.category {
            objective.category = one_line(category, 40, "category")?;
        }
        if let Some(why) = &self.why {
            objective.why = one_line(why, MAX_TEXT_CHARS, "why")?;
        }
        if let Some(status) = self.status {
            objective.status = status;
        }
        if let Some(next_step) = &self.next_step {
            objective.next_step = one_line(next_step, MAX_TEXT_CHARS, "next_step")?;
        }
        if let Some(due) = &self.due {
            objective.due = one_line(due, 40, "due")?;
        }
        if let Some(target) = &self.target {
            objective.target = match target {
                Some(target) => {
                    if !target.value.is_finite() {
                        bail!("target value must be a finite number");
                    }
                    Some(Target {
                        value: target.value,
                        unit: one_line(&target.unit, MAX_UNIT_CHARS, "unit")?,
                    })
                }
                None => None,
            };
        }
        objective.updated_at = now;
        Ok(())
    }
}

/// A new objective `id` with `change` applied; the title is required.
pub fn new_objective(id: String, change: &ObjectiveChange, now: u64) -> Result<Objective> {
    if change
        .title
        .as_deref()
        .is_none_or(|title| title.trim().is_empty())
    {
        bail!("a new objective needs a title");
    }
    let mut objective = Objective {
        id,
        title: String::new(),
        category: String::new(),
        why: String::new(),
        status: Status::Active,
        next_step: String::new(),
        due: String::new(),
        target: None,
        created_at: now,
        updated_at: now,
        log: Vec::new(),
    };
    change.apply(&mut objective, now)?;
    Ok(objective)
}

/// Record progress on `objective` at `now`.
pub fn record_progress(
    objective: &mut Objective,
    note: &str,
    value: Option<f64>,
    now: u64,
) -> Result<()> {
    let note = one_line(note, MAX_TEXT_CHARS, "note")?;
    if note.is_empty() {
        bail!("note must not be empty");
    }
    if value.is_some_and(|value| !value.is_finite()) {
        bail!("value must be a finite number");
    }
    objective.log.push(Progress {
        at: now,
        note,
        value,
    });
    if objective.log.len() > MAX_LOG_ENTRIES {
        let excess = objective.log.len() - MAX_LOG_ENTRIES;
        objective.log.drain(..excess);
    }
    objective.updated_at = now;
    Ok(())
}

/// The lines of the active objectives to send with a run, most recently
/// updated first, within `MAX_OBJECTIVE_INDEX_BYTES`; and how many active
/// objectives were left out.
pub fn active_summaries(objectives: &[Objective]) -> (Vec<String>, usize) {
    let mut active: Vec<&Objective> = objectives
        .iter()
        .filter(|objective| objective.status == Status::Active)
        .collect();
    active.sort_by_key(|objective| std::cmp::Reverse(objective.updated_at));
    let mut lines = Vec::new();
    let mut bytes = 0;
    let mut omitted = 0;
    for objective in active {
        let line = objective.summary();
        if bytes + line.len() + 3 > MAX_OBJECTIVE_INDEX_BYTES {
            omitted += 1;
            continue;
        }
        bytes += line.len() + 3;
        lines.push(line);
    }
    (lines, omitted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(title: &str) -> ObjectiveChange {
        ObjectiveChange {
            title: Some(title.into()),
            ..Default::default()
        }
    }

    #[test]
    fn objectives_need_a_title_and_take_bounded_fields() {
        assert!(new_objective("o1".into(), &ObjectiveChange::default(), 1).is_err());
        assert!(new_objective("o1".into(), &change("  "), 1).is_err());
        let mut objective = new_objective(
            "o1".into(),
            &ObjectiveChange {
                title: Some(" Save \n money ".into()),
                category: Some("money".into()),
                target: Some(Some(Target {
                    value: 100000.0,
                    unit: "yen".into(),
                })),
                ..Default::default()
            },
            10,
        )
        .unwrap();
        assert_eq!(objective.title, "Save money");
        assert_eq!(objective.status, Status::Active);
        assert!(change(&"t".repeat(MAX_TITLE_CHARS + 1))
            .apply(&mut objective, 11)
            .is_err());
        let bad_target = ObjectiveChange {
            target: Some(Some(Target {
                value: f64::NAN,
                unit: "yen".into(),
            })),
            ..Default::default()
        };
        assert!(bad_target.apply(&mut objective, 11).is_err());
        ObjectiveChange {
            target: Some(None),
            next_step: Some("Open a savings account".into()),
            ..Default::default()
        }
        .apply(&mut objective, 12)
        .unwrap();
        assert_eq!(objective.target, None);
        assert_eq!(objective.updated_at, 12);
    }

    #[test]
    fn progress_is_logged_and_summarized() {
        let mut objective = new_objective(
            "o2".into(),
            &ObjectiveChange {
                title: Some("Run 100 km".into()),
                category: Some("health".into()),
                due: Some("2026-12-31".into()),
                next_step: Some("Run 5 km on Saturday".into()),
                target: Some(Some(Target {
                    value: 100.0,
                    unit: "km".into(),
                })),
                ..Default::default()
            },
            1,
        )
        .unwrap();
        assert_eq!(
            objective.summary(),
            "[o2] Run 100 km (health); target 100 km; due 2026-12-31; next: Run 5 km on Saturday"
        );
        record_progress(&mut objective, "Ran 12.5 km", Some(12.5), 2).unwrap();
        record_progress(&mut objective, "Rested", None, 3).unwrap();
        assert_eq!(objective.current_value(), Some(12.5));
        assert!(objective.summary().contains("; 12.5 / 100 km;"));
        assert!(objective.summary().ends_with("; last progress: Rested"));
        assert!(record_progress(&mut objective, " ", None, 4).is_err());
        assert!(record_progress(&mut objective, "x", Some(f64::INFINITY), 4).is_err());
        for n in 0..MAX_LOG_ENTRIES {
            record_progress(&mut objective, &format!("day {n}"), None, 5).unwrap();
        }
        assert_eq!(objective.log.len(), MAX_LOG_ENTRIES);
        assert_eq!(objective.log[0].note, "day 0");
    }

    #[test]
    fn only_active_objectives_are_sent_most_recent_first() {
        let mut objectives = Vec::new();
        for (n, status) in [Status::Active, Status::Done, Status::Active, Status::Paused]
            .into_iter()
            .enumerate()
        {
            let mut objective = new_objective(
                format!("o{n}"),
                &change(&format!("Objective {n}")),
                n as u64,
            )
            .unwrap();
            objective.status = status;
            objectives.push(objective);
        }
        let (lines, omitted) = active_summaries(&objectives);
        assert_eq!(lines, ["[o2] Objective 2", "[o0] Objective 0"]);
        assert_eq!(omitted, 0);
        assert_eq!(Status::parse("done").unwrap(), Status::Done);
        assert!(Status::parse("finished").is_err());
    }
}
