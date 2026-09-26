//! Human-readable and JSON output of runs, plans, and progress events.

use crate::{
    application::agent::{AgentEvent, AgentResult},
    domain::plan::{RunOutcome, TaskPlan},
};
use anyhow::Result;

pub(super) fn format_result(result: &AgentResult, as_json: bool) -> Result<String> {
    if as_json {
        Ok(serde_json::to_string(&serde_json::json!({
            "text": result.text,
            "response_id": result.response_id,
            "events": result.events,
            "outcome": result.outcome,
            "plan": result.plan,
            "usage": result.usage,
            "stop_reason": result.stop_reason,
        }))?)
    } else {
        if result.outcome == RunOutcome::Completed {
            Ok(result.text.clone())
        } else {
            Ok(format!("{}\n\n{}", result.text, format_plan(&result.plan)))
        }
    }
}

pub(super) fn format_plan(plan: &TaskPlan) -> String {
    let mut lines = vec![format!(
        "Plan: {:?} (revision {})",
        plan.outcome(),
        plan.revision
    )];
    for step in &plan.steps {
        lines.push(format!(
            "  [{:?}] {}: {}{}",
            step.status,
            step.id,
            step.description,
            step.detail
                .as_ref()
                .map(|detail| format!(" — {detail}"))
                .unwrap_or_default()
        ));
    }
    lines.join("\n")
}

pub(super) fn print_event(event: &AgentEvent, verbose: bool) {
    match event {
        AgentEvent::ContextCompacted { record, .. } => eprintln!(
            "[context compacted] {} -> {} bytes",
            record.before_bytes, record.after_bytes
        ),
        AgentEvent::UsageUpdated { usage, .. } => {
            if verbose {
                eprintln!("[usage] {} reported tokens", usage.total_tokens);
            }
        }
        AgentEvent::ExecutionStopped { reason, .. } => eprintln!("[execution stopped] {reason:?}"),
        AgentEvent::PlanUpdated { plan, .. } => eprintln!("{}", format_plan(plan)),
        AgentEvent::AssistantProgress { text, .. } => eprintln!("[agent] {text}"),
        AgentEvent::ReasoningSummary { text, .. } => eprintln!("[reasoning] {text}"),
        AgentEvent::LocalToolCall {
            name, arguments, ..
        } => {
            if verbose {
                eprintln!("[tool] {name} {arguments}");
            } else {
                eprintln!("[tool] {name}");
            }
        }
        AgentEvent::LocalToolResult { name, output, .. } => {
            print_tool_result(&format!("[tool result] {name}"), output, verbose);
        }
        AgentEvent::LocalToolBlocked { name, .. } => {
            eprintln!("[tool blocked] {name}");
        }
        AgentEvent::McpToolCall {
            server_label,
            tool_name,
            arguments,
            ..
        } => {
            if verbose {
                eprintln!("[mcp call] {server_label}:{tool_name} {arguments}");
            } else {
                eprintln!("[mcp call] {server_label}:{tool_name}");
            }
        }
        AgentEvent::McpToolResult {
            server_label,
            tool_name,
            output,
            ..
        } => {
            print_tool_result(
                &format!("[mcp result] {server_label}:{tool_name}"),
                output,
                verbose,
            );
        }
        AgentEvent::McpToolBlocked {
            server_label,
            tool_name,
            ..
        } => {
            eprintln!("[mcp blocked] {server_label}:{tool_name}");
        }
        AgentEvent::McpApproval {
            server_label,
            tool_name,
            approved,
            ..
        } => {
            eprintln!(
                "[mcp approval] {server_label}:{tool_name} -> {}",
                if *approved { "approved" } else { "denied" }
            );
        }
        AgentEvent::ToolSearch { query, results, .. } => {
            eprintln!("[tool search] {query} -> {} result(s)", results.len());
        }
    }
}

fn print_tool_result(label: &str, output: &serde_json::Value, verbose: bool) {
    if verbose {
        eprintln!("{label} {output}");
    } else if output.get("error").is_some() || output["isError"] == true {
        eprintln!("{label} error");
    } else {
        eprintln!("{label} received");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::usage::{StopReason, UsageSummary};

    #[test]
    fn json_output_round_trips_text_and_events() {
        let result = AgentResult {
            usage: UsageSummary::default(),
            stop_reason: StopReason::FinalAnswer,
            outcome: RunOutcome::Completed,
            plan: TaskPlan::default(),
            text: "日本語の結果\n\"quoted\"".into(),
            response_id: "resp_123".into(),
            events: vec![AgentEvent::LocalToolBlocked {
                round: 0,
                name: "write".into(),
            }],
        };
        let output: serde_json::Value =
            serde_json::from_str(&format_result(&result, true).unwrap()).unwrap();
        assert_eq!(output["text"], result.text);
        assert_eq!(output["response_id"], "resp_123");
        assert_eq!(output["events"][0]["type"], "local_tool_blocked");
        assert_eq!(format_result(&result, false).unwrap(), result.text);
    }
}
