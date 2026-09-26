//! Run-scoped task plans. Revisions prevent concurrent updates from silently
//! overwriting each other; session checkpoints persist the accepted state.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

pub const TASK_PLAN_NAME: &str = "task_plan";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    InProgress,
    Completed,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    pub id: String,
    pub description: String,
    pub status: StepStatus,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskPlan {
    pub revision: u64,
    pub explanation: Option<String>,
    pub steps: Vec<PlanStep>,
}

/// Describes the recorded plan, not an independent assessment of task quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Completed,
    Blocked,
    Incomplete,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanArguments {
    expected_revision: Option<u64>,
    steps: Option<Vec<PlanStep>>,
    explanation: Option<String>,
}

impl TaskPlan {
    pub fn outcome(&self) -> RunOutcome {
        if self
            .steps
            .iter()
            .any(|step| matches!(step.status, StepStatus::Pending | StepStatus::InProgress))
        {
            RunOutcome::Incomplete
        } else if self
            .steps
            .iter()
            .any(|step| step.status == StepStatus::Blocked)
        {
            RunOutcome::Blocked
        } else {
            RunOutcome::Completed
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.steps.len() > 50 {
            bail!("a task plan can contain at most 50 steps");
        }
        if self
            .explanation
            .as_ref()
            .is_some_and(|text| text.len() > 4000)
        {
            bail!("plan explanation exceeds 4000 bytes");
        }
        let mut ids = HashSet::new();
        let mut in_progress = 0;
        for step in &self.steps {
            if step.id.is_empty()
                || step.id.len() > 64
                || !step
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            {
                bail!("step id must contain 1 to 64 ASCII letters, digits, '_' or '-'");
            }
            if !ids.insert(&step.id) {
                bail!("duplicate plan step id: {}", step.id);
            }
            if step.description.trim().is_empty() || step.description.len() > 2000 {
                bail!("step description must contain 1 to 2000 bytes of text");
            }
            if step.detail.as_ref().is_some_and(|text| text.len() > 4000) {
                bail!("step detail exceeds 4000 bytes");
            }
            if step.status == StepStatus::Blocked
                && step
                    .detail
                    .as_ref()
                    .is_none_or(|text| text.trim().is_empty())
            {
                bail!("blocked step '{}' requires a reason in detail", step.id);
            }
            in_progress += usize::from(step.status == StepStatus::InProgress);
        }
        if in_progress > 1 {
            bail!("only one plan step may be in_progress at a time");
        }
        Ok(())
    }

    /// Returns whether this call changed the plan. Invalid updates never mutate it.
    pub(crate) fn apply(&mut self, arguments: &Value) -> Result<bool> {
        let arguments: PlanArguments =
            serde_json::from_value(arguments.clone()).context("invalid task_plan arguments")?;
        let Some(steps) = arguments.steps else {
            return Ok(false);
        };
        let expected = arguments.expected_revision.context(
            "read task_plan with steps=null, then provide expected_revision when updating",
        )?;
        if expected != self.revision {
            bail!("plan conflict: expected revision {expected}, current revision {}; read the plan and retry", self.revision);
        }
        if steps.is_empty() {
            bail!("an updated plan must contain at least one step");
        }
        let next = Self {
            revision: self
                .revision
                .checked_add(1)
                .context("plan revision exhausted")?,
            explanation: arguments.explanation,
            steps,
        };
        next.validate()?;
        let replanned = self.steps.iter().any(|old| {
            !next
                .steps
                .iter()
                .any(|new| new.id == old.id && new.description == old.description)
        });
        if replanned
            && next
                .explanation
                .as_ref()
                .is_none_or(|text| text.trim().is_empty())
        {
            bail!("changing or removing existing steps requires an explanation");
        }
        *self = next;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(id: &str, status: &str) -> Value {
        json!({"id":id,"description":"Verify the change","status":status,"detail":null})
    }

    #[test]
    fn revisions_and_validation_preserve_the_last_accepted_plan() {
        let mut plan = TaskPlan::default();
        plan.apply(&json!({"expected_revision":0,"steps":[step("verify", "in_progress")]}))
            .unwrap();
        let saved = plan.clone();
        for arguments in [
            json!({"expected_revision":0,"steps":[step("verify","completed")]}),
            json!({"expected_revision":1,"steps":[step("same","pending"),step("same","pending")]}),
            json!({"expected_revision":1,"steps":[step("one","in_progress"),step("two","in_progress")]}),
            json!({"expected_revision":1,"steps":[step("verify","blocked")]}),
            json!({"expected_revision":1,"steps":[]}),
            json!({"expected_revision":1,"steps":[step("replacement","completed")]}),
        ] {
            assert!(plan.apply(&arguments).is_err());
            assert_eq!(plan, saved);
        }
        assert!(!plan.apply(&json!({"steps":null})).unwrap());
        assert_eq!(plan.outcome(), RunOutcome::Incomplete);
        plan.apply(&json!({"expected_revision":1,"steps":[{"id":"verify","description":"Verify the change","status":"blocked","detail":"Test database unavailable"}]})).unwrap();
        assert_eq!(plan.outcome(), RunOutcome::Blocked);
        plan.apply(&json!({"expected_revision":2,"steps":[step("verify","completed")]}))
            .unwrap();
        assert_eq!(plan.outcome(), RunOutcome::Completed);
    }
}
