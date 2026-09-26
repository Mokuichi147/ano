//! Human-readable and JSON output of runs, plans, and progress events.

use crate::{
    application::agent::{AgentEvent, AgentResult},
    domain::plan::{RunOutcome, TaskPlan},
};
use anyhow::Result;
use std::{
    io::IsTerminal,
    sync::{LazyLock, Mutex, PoisonError},
};
use termimad::{crossterm::style::Stylize, Alignment, MadSkin};

/// How the answer text is printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TextFormat {
    /// The Markdown as the model wrote it, e.g. for a pipe.
    Raw,
    /// Markdown formatted for a terminal `width` columns wide.
    Terminal { width: usize, color: bool },
}

impl TextFormat {
    /// Format for the terminal when stdout is one, unless `raw` is requested.
    /// `NO_COLOR` keeps the layout but drops colors and text styles.
    pub fn for_stdout(raw: bool) -> Self {
        use std::io::IsTerminal;
        if raw || !std::io::stdout().is_terminal() {
            return Self::Raw;
        }
        Self::Terminal {
            width: usize::from(termimad::terminal_size().0),
            color: std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty()),
        }
    }

    fn apply(self, markdown: &str) -> String {
        match self {
            Self::Raw => markdown.to_string(),
            Self::Terminal { width, color } => {
                let mut skin = if color {
                    MadSkin::default()
                } else {
                    MadSkin::no_style()
                };
                for header in &mut skin.headers {
                    header.align = Alignment::Left;
                }
                skin.text(&close_tables(markdown), Some(width))
                    .to_string()
                    .trim_end()
                    .to_string()
            }
        }
    }
}

/// Rewrite GitHub-style tables into the form termimad draws in full: rows
/// get pipes at both ends, and the delimiter row is repeated above and below
/// the table, which termimad needs to draw its top and bottom borders.
/// Fenced code blocks are left as they are.
fn close_tables(markdown: &str) -> String {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut output = Vec::with_capacity(lines.len());
    let mut fence: Option<&str> = None;
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim_start();
        if let Some(marker) = fence {
            if trimmed.starts_with(marker) {
                fence = None;
            }
        } else if let Some(marker) = ["```", "~~~"]
            .into_iter()
            .find(|marker| trimmed.starts_with(marker))
        {
            fence = Some(marker);
        } else if line.contains('|')
            && lines
                .get(index + 1)
                .is_some_and(|next| is_delimiter_row(next))
        {
            let delimiter = with_outer_pipes(lines[index + 1]);
            output.push(delimiter.clone());
            output.push(with_outer_pipes(line));
            output.push(delimiter.clone());
            index += 2;
            while let Some(row) = lines
                .get(index)
                .filter(|row| row.contains('|') && !row.trim().is_empty())
            {
                output.push(with_outer_pipes(row));
                index += 1;
            }
            output.push(delimiter);
            continue;
        }
        output.push(line.to_string());
        index += 1;
    }
    output.join("\n")
}

/// A row such as `|---|:-:|` or `--- | ---` below a table header.
fn is_delimiter_row(line: &str) -> bool {
    let cells: Vec<&str> = line.trim().trim_matches('|').split('|').collect();
    line.contains('-')
        && (line.contains('|') || cells.len() > 1)
        && cells.iter().all(|cell| {
            let cell = cell.trim().trim_start_matches(':').trim_end_matches(':');
            !cell.is_empty() && cell.chars().all(|character| character == '-')
        })
}

fn with_outer_pipes(row: &str) -> String {
    let row = row.trim();
    let start = if row.starts_with('|') { "" } else { "| " };
    let end = if row.ends_with('|') && !row.ends_with("\\|") {
        ""
    } else {
        " |"
    };
    format!("{start}{row}{end}")
}

pub(super) fn format_result(
    result: &AgentResult,
    as_json: bool,
    format: TextFormat,
) -> Result<String> {
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
        let text = format.apply(&result.text);
        if result.outcome == RunOutcome::Completed {
            Ok(text)
        } else {
            Ok(format!("{text}\n\n{}", format_plan(&result.plan)))
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

/// Progress lines held back while an approval prompt waits for an answer,
/// or `None` when lines are printed right away.
static HELD_PROGRESS: Mutex<Option<Vec<String>>> = Mutex::new(None);

macro_rules! progress {
    ($($argument:tt)*) => {
        emit_progress(format!($($argument)*))
    };
}

fn emit_progress(line: String) {
    let mut held = HELD_PROGRESS.lock().unwrap_or_else(PoisonError::into_inner);
    match held.as_mut() {
        Some(lines) => lines.push(line),
        None => print_progress(&line),
    }
}

/// Print a progress line dimmed on a terminal, so that it stands apart from
/// the answer. `NO_COLOR` turns the styling off.
fn print_progress(line: &str) {
    static DIM: LazyLock<bool> = LazyLock::new(|| {
        std::io::stderr().is_terminal()
            && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
    });
    if *DIM {
        for line in line.lines() {
            eprintln!("{}", line.dim());
        }
    } else {
        eprintln!("{line}");
    }
}

/// Holds progress lines back until the guard is dropped, so that events of
/// tool calls running in parallel do not break into a prompt.
pub(super) struct ProgressHold(());

impl ProgressHold {
    pub(super) fn start() -> Self {
        HELD_PROGRESS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(Vec::new);
        Self(())
    }
}

impl Drop for ProgressHold {
    fn drop(&mut self) {
        let held = HELD_PROGRESS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        for line in held.into_iter().flatten() {
            print_progress(&line);
        }
    }
}

pub(super) fn print_event(event: &AgentEvent, verbose: bool) {
    match event {
        AgentEvent::ContextCompacted { record, .. } => progress!(
            "[context compacted] {} -> {} bytes",
            record.before_bytes,
            record.after_bytes
        ),
        AgentEvent::UsageUpdated { usage, .. } => {
            if verbose {
                progress!("[usage] {} reported tokens", usage.total_tokens);
            }
        }
        AgentEvent::ExecutionStopped { reason, .. } => progress!("[execution stopped] {reason:?}"),
        AgentEvent::PlanUpdated { plan, .. } => progress!("{}", format_plan(plan)),
        AgentEvent::AssistantProgress { text, .. } => progress!("[agent] {text}"),
        AgentEvent::ReasoningSummary { text, .. } => progress!("[reasoning] {text}"),
        AgentEvent::LocalToolCall {
            name, arguments, ..
        } => {
            if verbose {
                progress!("[tool] {name} {arguments}");
            } else {
                progress!("[tool] {name}");
            }
        }
        AgentEvent::LocalToolResult { name, output, .. } => {
            print_tool_result(&format!("[tool result] {name}"), output, verbose);
        }
        AgentEvent::LocalToolBlocked { name, .. } => {
            progress!("[tool blocked] {name}");
        }
        AgentEvent::LocalToolApproval {
            name,
            approved,
            reason,
            ..
        } => {
            let decision = if *approved { "approved" } else { "denied" };
            match reason {
                Some(reason) => progress!("[approval] {name} -> {decision} ({reason})"),
                None => progress!("[approval] {name} -> {decision}"),
            }
        }
        AgentEvent::McpToolCall {
            server_label,
            tool_name,
            arguments,
            ..
        } => {
            if verbose {
                progress!("[mcp call] {server_label}:{tool_name} {arguments}");
            } else {
                progress!("[mcp call] {server_label}:{tool_name}");
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
            progress!("[mcp blocked] {server_label}:{tool_name}");
        }
        AgentEvent::McpApproval {
            server_label,
            tool_name,
            approved,
            reason,
            ..
        } => {
            let decision = if *approved { "approved" } else { "denied" };
            match reason {
                Some(reason) => {
                    progress!("[mcp approval] {server_label}:{tool_name} -> {decision} ({reason})")
                }
                None => progress!("[mcp approval] {server_label}:{tool_name} -> {decision}"),
            }
        }
        AgentEvent::ToolSearch { query, results, .. } => {
            progress!("[tool search] {query} -> {} result(s)", results.len());
        }
        AgentEvent::SubagentStarted { task, .. } => {
            let summary = task.lines().next().unwrap_or_default();
            let summary = match summary.char_indices().nth(80) {
                Some((index, _)) => format!("{}…", &summary[..index]),
                None => summary.to_string(),
            };
            progress!("[subagent] started: {summary}");
        }
        AgentEvent::SubagentFinished {
            outcome,
            usage,
            error,
            ..
        } => match (outcome, error) {
            (_, Some(error)) => progress!("[subagent] failed: {error}"),
            (Some(outcome), None) => progress!(
                "[subagent] finished ({outcome:?}, {} tokens)",
                usage.total_tokens
            ),
            (None, None) => progress!("[subagent] finished"),
        },
    }
}

fn print_tool_result(label: &str, output: &serde_json::Value, verbose: bool) {
    if verbose {
        progress!("{label} {output}");
    } else if output.get("error").is_some() || output["isError"] == true {
        progress!("{label} error");
    } else {
        progress!("{label} received");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::usage::{StopReason, UsageSummary};

    #[test]
    fn progress_is_held_while_a_prompt_waits() {
        let hold = ProgressHold::start();
        progress!("[mcp result] web:search received");
        assert_eq!(
            HELD_PROGRESS.lock().unwrap().as_deref(),
            Some(&["[mcp result] web:search received".to_string()][..])
        );
        drop(hold);
        assert!(HELD_PROGRESS.lock().unwrap().is_none());
    }

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
            serde_json::from_str(&format_result(&result, true, TextFormat::Raw).unwrap()).unwrap();
        assert_eq!(output["text"], result.text);
        assert_eq!(output["response_id"], "resp_123");
        assert_eq!(output["events"][0]["type"], "local_tool_blocked");
        assert_eq!(
            format_result(&result, false, TextFormat::Raw).unwrap(),
            result.text
        );
    }

    #[test]
    fn terminal_format_renders_markdown_markers() {
        let format = TextFormat::Terminal {
            width: 40,
            color: false,
        };
        let text =
            format.apply("## 見出し\n\n- **太字**の項目\n\n| 作品 | 理由 |\n|---|---|\n| A | B |");
        assert!(!text.contains("##"));
        assert!(!text.contains("**"));
        assert!(text.contains("見出し"));
        assert!(text.contains("• 太字の項目"));
        assert!(text.contains("│"));
        assert!(text.contains("┌"));
        assert!(text.contains("└"));
    }

    #[test]
    fn tables_get_outer_pipes_and_closing_rules_outside_code_blocks() {
        let markdown = "前置き\n作品 | 理由\n:--- | ---\nA | B\n\n```\n| a | b |\n|---|---|\n```";
        assert_eq!(
            close_tables(markdown),
            "前置き\n| :--- | --- |\n| 作品 | 理由 |\n| :--- | --- |\n| A | B |\n| :--- | --- |\n\n```\n| a | b |\n|---|---|\n```"
        );
        assert!(!is_delimiter_row("---"));
        assert!(!is_delimiter_row("| a | b |"));
        assert!(is_delimiter_row("|:-:|"));
    }
}
