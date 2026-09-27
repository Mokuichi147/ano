//! Run-scoped task plans. Revisions prevent concurrent updates from silently
//! overwriting each other; session checkpoints persist the accepted state.
//!
//! A plan can carry a goal: the state the work must reach and the acceptance
//! criteria that show it has. With a goal, the work is complete only when
//! every criterion is verified as met, not merely when the steps are done.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionStatus {
    /// Not yet verified.
    Pending,
    /// Verified as met; `evidence` says how.
    Met,
    /// Cannot be met; `evidence` says why.
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceCriterion {
    pub id: String,
    pub description: String,
    pub status: CriterionStatus,
    pub evidence: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskGoal {
    /// The state the work must reach.
    pub objective: String,
    pub acceptance: Vec<AcceptanceCriterion>,
    /// Set by the user. The agent cannot change the objective.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub by_user: bool,
}

impl TaskGoal {
    /// A goal set by the user. The agent derives the acceptance criteria
    /// from the objective, which may itself state conditions.
    pub fn from_user(objective: &str) -> Result<Self> {
        let goal = Self {
            objective: objective.trim().to_string(),
            acceptance: Vec::new(),
            by_user: true,
        };
        goal.validate(true)?;
        Ok(goal)
    }

    /// `allow_empty` accepts a goal whose criteria the agent has yet to
    /// define, as a user-set goal starts.
    fn validate(&self, allow_empty: bool) -> Result<()> {
        if self.objective.trim().is_empty() || self.objective.len() > 4000 {
            bail!("goal objective must contain 1 to 4000 bytes of text");
        }
        if self.acceptance.len() > MAX_CRITERIA || (!allow_empty && self.acceptance.is_empty()) {
            bail!("a goal needs 1 to {MAX_CRITERIA} acceptance criteria");
        }
        let mut ids = HashSet::new();
        for criterion in &self.acceptance {
            validate_id(&criterion.id, "criterion")?;
            if !ids.insert(&criterion.id) {
                bail!("duplicate acceptance criterion id: {}", criterion.id);
            }
            if criterion.description.trim().is_empty() || criterion.description.len() > 2000 {
                bail!("acceptance criterion description must contain 1 to 2000 bytes of text");
            }
            if criterion
                .evidence
                .as_ref()
                .is_some_and(|text| text.len() > 4000)
            {
                bail!("acceptance criterion evidence exceeds 4000 bytes");
            }
            if criterion.status != CriterionStatus::Pending
                && criterion
                    .evidence
                    .as_ref()
                    .is_none_or(|text| text.trim().is_empty())
            {
                bail!(
                    "criterion '{}' marked {:?} requires evidence",
                    criterion.id,
                    criterion.status
                );
            }
        }
        Ok(())
    }

    /// Criteria not yet verified as met or blocked.
    pub fn pending(&self) -> impl Iterator<Item = &AcceptanceCriterion> {
        self.acceptance
            .iter()
            .filter(|criterion| criterion.status == CriterionStatus::Pending)
    }
}

const MAX_CRITERIA: usize = 20;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskPlan {
    pub revision: u64,
    pub explanation: Option<String>,
    pub steps: Vec<PlanStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<TaskGoal>,
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
    #[serde(default)]
    goal: Option<GoalArguments>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalArguments {
    /// `None` keeps the current objective.
    objective: Option<String>,
    acceptance: Vec<AcceptanceCriterion>,
}

fn validate_id(id: &str, what: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        bail!("{what} id must contain 1 to 64 ASCII letters, digits, '_' or '-'");
    }
    Ok(())
}

impl TaskPlan {
    /// A new plan for a goal the user set. The revision keeps counting, so an
    /// update prepared against the previous plan is rejected.
    pub fn for_user_goal(&self, goal: TaskGoal) -> Self {
        Self {
            revision: self.revision.saturating_add(1),
            explanation: None,
            steps: Vec::new(),
            goal: Some(goal),
        }
    }

    /// The same plan without its goal, e.g. when the user clears it.
    pub fn without_goal(&self) -> Self {
        Self {
            revision: self.revision.saturating_add(1),
            goal: None,
            ..self.clone()
        }
    }

    pub fn outcome(&self) -> RunOutcome {
        let steps = self.steps_outcome();
        let Some(goal) = &self.goal else {
            return steps;
        };
        if steps == RunOutcome::Incomplete
            || goal.acceptance.is_empty()
            || goal.pending().next().is_some()
        {
            RunOutcome::Incomplete
        } else if steps == RunOutcome::Blocked
            || goal
                .acceptance
                .iter()
                .any(|criterion| criterion.status == CriterionStatus::Blocked)
        {
            RunOutcome::Blocked
        } else {
            RunOutcome::Completed
        }
    }

    fn steps_outcome(&self) -> RunOutcome {
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
        if let Some(goal) = &self.goal {
            goal.validate(goal.by_user)?;
        }
        let mut ids = HashSet::new();
        let mut in_progress = 0;
        for step in &self.steps {
            validate_id(&step.id, "step")?;
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
        if arguments.steps.is_none() && arguments.goal.is_none() {
            return Ok(false);
        }
        let expected = arguments.expected_revision.context(
            "read task_plan with steps=null and goal=null, then provide expected_revision when updating",
        )?;
        if expected != self.revision {
            bail!("plan conflict: expected revision {expected}, current revision {}; read the plan and retry", self.revision);
        }
        let steps = match arguments.steps {
            Some(steps) if steps.is_empty() => {
                bail!("an updated plan must contain at least one step")
            }
            Some(steps) => steps,
            // goal only: keep the steps.
            None => self.steps.clone(),
        };
        let goal = match arguments.goal {
            Some(goal) => Some(self.next_goal(goal)?),
            None => self.goal.clone(),
        };
        let next = Self {
            revision: self
                .revision
                .checked_add(1)
                .context("plan revision exhausted")?,
            explanation: arguments.explanation,
            steps,
            goal,
        };
        next.validate()?;
        if let Some(goal) = &next.goal {
            if goal.acceptance.is_empty() {
                bail!("define at least one acceptance criterion for the goal");
            }
        }
        let replanned = self.steps.iter().any(|old| {
            !next
                .steps
                .iter()
                .any(|new| new.id == old.id && new.description == old.description)
        });
        let regoaled = match (&self.goal, &next.goal) {
            (Some(old), Some(new)) => {
                old.objective != new.objective
                    || old.acceptance.iter().any(|old| {
                        !new.acceptance
                            .iter()
                            .any(|new| new.id == old.id && new.description == old.description)
                    })
            }
            _ => false,
        };
        if (replanned || regoaled)
            && next
                .explanation
                .as_ref()
                .is_none_or(|text| text.trim().is_empty())
        {
            bail!("changing or removing existing steps, the goal, or its criteria requires an explanation");
        }
        *self = next;
        Ok(true)
    }

    /// The goal after an update from the agent. The objective of a goal set
    /// by the user cannot change; the agent defines and records the criteria.
    ///
    /// The objective of a user's goal is kept whatever the agent sends, so a
    /// model that cannot echo it exactly is not stuck on rejected updates.
    fn next_goal(&self, arguments: GoalArguments) -> Result<TaskGoal> {
        let by_user = self.goal.as_ref().is_some_and(|goal| goal.by_user);
        let objective = match (&self.goal, arguments.objective) {
            (Some(current), _) if current.by_user => current.objective.clone(),
            (_, Some(objective)) => objective.trim().to_string(),
            (Some(current), None) => current.objective.clone(),
            (None, None) => bail!("a new goal needs an objective"),
        };
        Ok(TaskGoal {
            objective,
            acceptance: arguments.acceptance,
            by_user,
        })
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

    fn criterion(id: &str, status: &str, evidence: Option<&str>) -> Value {
        json!({"id":id,"description":format!("{id} holds"),"status":status,"evidence":evidence})
    }

    fn goal(objective: &str, criteria: Vec<Value>) -> Value {
        json!({"objective":objective,"acceptance":criteria})
    }

    #[test]
    fn a_goal_is_complete_only_when_every_criterion_is_verified() {
        let mut plan = TaskPlan::default();
        plan.apply(&json!({"expected_revision":0,"explanation":null,"steps":[step("fix","completed")],
            "goal":goal("Tests pass",vec![criterion("tests","pending",None),criterion("lint","pending",None)])}))
            .unwrap();
        // Finished steps do not complete an unverified goal.
        assert_eq!(plan.outcome(), RunOutcome::Incomplete);

        // Met or blocked needs evidence; invalid updates change nothing.
        let saved = plan.clone();
        for arguments in [
            json!({"expected_revision":1,"steps":null,"goal":goal("Tests pass",vec![criterion("tests","met",None),criterion("lint","pending",None)])}),
            json!({"expected_revision":1,"steps":null,"goal":goal("Tests pass",vec![])}),
            json!({"expected_revision":1,"steps":null,"goal":goal("Tests pass",vec![criterion("x","pending",None),criterion("x","pending",None)])}),
            // Dropping a criterion or changing the objective needs a reason.
            json!({"expected_revision":1,"steps":null,"goal":goal("Tests pass",vec![criterion("tests","pending",None)])}),
            json!({"expected_revision":1,"steps":null,"goal":goal("Something else",vec![criterion("tests","pending",None),criterion("lint","pending",None)])}),
        ] {
            assert!(plan.apply(&arguments).is_err(), "{arguments}");
            assert_eq!(plan, saved);
        }

        // A goal-only update keeps the steps.
        plan.apply(&json!({"expected_revision":1,"steps":null,"goal":goal("Tests pass",vec![
            criterion("tests","met",Some("cargo test: 42 passed")),criterion("lint","blocked",Some("clippy is not installed"))])}))
            .unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.outcome(), RunOutcome::Blocked);
        plan.apply(&json!({"expected_revision":2,"steps":null,"goal":goal("Tests pass",vec![
            criterion("tests","met",Some("cargo test: 42 passed")),criterion("lint","met",Some("clippy: no warnings"))])}))
            .unwrap();
        assert_eq!(plan.outcome(), RunOutcome::Completed);
        // Steps left open still keep the work incomplete.
        plan.apply(&json!({"expected_revision":3,"steps":[step("fix","completed"),step("docs","pending")]}))
            .unwrap();
        assert_eq!(plan.outcome(), RunOutcome::Incomplete);
    }

    #[test]
    fn the_agent_cannot_change_a_goal_the_user_set() {
        let user_goal = TaskGoal::from_user(" README を直し、リンク切れをなくす ").unwrap();
        assert_eq!(user_goal.objective, "README を直し、リンク切れをなくす");
        assert!(user_goal.acceptance.is_empty());
        assert!(TaskGoal::from_user("  ").is_err());
        let mut plan = TaskPlan {
            revision: 4,
            steps: vec![PlanStep {
                id: "old".into(),
                description: "Earlier work".into(),
                status: StepStatus::Pending,
                detail: None,
            }],
            ..TaskPlan::default()
        }
        .for_user_goal(user_goal);
        // The earlier steps are closed and the revision keeps counting.
        assert_eq!((plan.revision, plan.steps.len()), (5, 0));
        // Criteria are still to be defined.
        assert_eq!(plan.outcome(), RunOutcome::Incomplete);

        // The agent defines the criteria; the objective it sends is ignored,
        // including a slightly different copy.
        let links = json!({"id":"links","description":"リンク切れがない","status":"pending","evidence":null});
        plan.apply(
            &json!({"expected_revision":5,"explanation":null,"steps":null,
            "goal":goal("README を直し,リンク切れをなくす",vec![links])}),
        )
        .unwrap();
        let goal_after = plan.goal.as_ref().unwrap();
        assert!(goal_after.by_user);
        assert_eq!(goal_after.objective, "README を直し、リンク切れをなくす");
        // A null objective keeps it too.
        plan.apply(&json!({"expected_revision":6,"explanation":null,"steps":null,
            "goal":{"objective":null,"acceptance":[
                {"id":"links","description":"リンク切れがない","status":"met","evidence":"全リンクを確認"},
                criterion("typos","met",Some("誤字なし"))]}}))
            .unwrap();
        assert_eq!(
            plan.goal.as_ref().unwrap().objective,
            "README を直し、リンク切れをなくす"
        );
        assert_eq!(plan.outcome(), RunOutcome::Completed);
        // An agent's own new goal needs an objective.
        let mut own = TaskPlan::default();
        assert!(own
            .apply(
                &json!({"expected_revision":0,"explanation":null,"steps":null,
                "goal":{"objective":null,"acceptance":[criterion("x","pending",None)]}})
            )
            .is_err());

        let cleared = plan.without_goal();
        assert!(cleared.goal.is_none());
        assert_eq!(cleared.revision, plan.revision + 1);
        // Plans saved before goals existed still load.
        let old: TaskPlan =
            serde_json::from_value(json!({"revision":1,"explanation":null,"steps":[]})).unwrap();
        assert!(old.goal.is_none());
        assert!(!serde_json::to_string(&old).unwrap().contains("goal"));
    }
}
