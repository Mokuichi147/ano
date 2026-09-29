//! Human-readable and JSON output of runs, plans, and progress events.

use crate::{
    application::{
        agent::{AgentEvent, AgentResult},
        ports::ResponseDelta,
    },
    domain::plan::{CriterionStatus, RunOutcome, StepStatus, TaskGoal, TaskPlan},
};
use anyhow::Result;
use std::{
    io::{IsTerminal, Write},
    sync::{LazyLock, Mutex, PoisonError},
};
use termimad::{crossterm::style::Stylize, Alignment, MadSkin};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
        // A terminal that reports no usable size (e.g. a bare pseudo
        // terminal) gets a conventional width instead of a squeezed layout.
        let width = usize::from(termimad::terminal_size().0);
        Self::Terminal {
            width: if width < 20 { 80 } else { width },
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
        // A streamed answer is already on the screen.
        let text = if result.streamed {
            String::new()
        } else {
            format.apply(&result.text)
        };
        // A goal is always reported, so that its verification can be seen.
        if result.outcome == RunOutcome::Completed && result.plan.goal.is_none() {
            Ok(text)
        } else {
            Ok(format!("{text}\n\n{}", format_plan(&result.plan))
                .trim_start()
                .to_string())
        }
    }
}

/// Prints message text on stdout while the model generates it.
///
/// Raw text is printed as it arrives. Formatted text is printed one Markdown
/// block at a time: a block ends at a blank line outside a code fence, so a
/// paragraph, list, table, or code block is formatted once it is complete.
pub(super) struct AnswerStream {
    format: TextFormat,
    /// Show that the model is reasoning, as a progress line.
    show_progress: bool,
    state: Mutex<StreamState>,
}

#[derive(Default)]
struct StreamState {
    /// Text after the last complete line.
    partial: String,
    /// Complete lines of the current block.
    block: Vec<String>,
    /// The fence marker while inside a fenced code block.
    fence: Option<&'static str>,
    /// Something of the current message has been printed.
    started: bool,
    /// The reasoning indicator is shown for the current response.
    thinking: bool,
}

impl AnswerStream {
    pub(super) fn new(format: TextFormat, show_progress: bool) -> Self {
        Self {
            format,
            show_progress,
            state: Mutex::new(StreamState::default()),
        }
    }

    /// Called on agent events, which come between responses: the next
    /// response announces its reasoning again.
    pub(super) fn end_response(&self) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .thinking = false;
    }

    pub(super) fn push(&self, delta: ResponseDelta<'_>) {
        if let ResponseDelta::Reasoning(_) = delta {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if !state.thinking && self.show_progress {
                // Reasoning can take long; show once that the model works.
                state.thinking = true;
                emit_status("[thinking]".to_string());
            }
            return;
        }
        let out = self.render(delta);
        if out.is_empty() {
            return;
        }
        clear_status();
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(out.as_bytes()).ok();
        stdout.flush().ok();
    }

    /// Drop a message cut off by an interruption, so that the next message
    /// starts cleanly.
    pub(super) fn abandon(&self) {
        clear_status();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.started || !state.partial.is_empty() || !state.block.is_empty() {
            println!();
        }
        *state = StreamState::default();
    }

    /// The text to print for `delta`.
    fn render(&self, delta: ResponseDelta<'_>) -> String {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out = String::new();
        state.thinking = false;
        match delta {
            ResponseDelta::Reasoning(_) => {}
            ResponseDelta::Text(text) if self.format == TextFormat::Raw => {
                if !state.started && !text.is_empty() {
                    // A blank line sets the answer apart from the progress.
                    out.push('\n');
                    state.started = true;
                }
                out.push_str(text);
            }
            ResponseDelta::Text(text) => {
                state.partial.push_str(text);
                while let Some(end) = state.partial.find('\n') {
                    let line = state.partial[..end].trim_end_matches('\r').to_string();
                    state.partial.drain(..=end);
                    self.line(&mut state, line, &mut out);
                }
            }
            ResponseDelta::MessageDone => {
                if self.format != TextFormat::Raw {
                    let rest = std::mem::take(&mut state.partial);
                    if !rest.is_empty() {
                        self.line(&mut state, rest, &mut out);
                    }
                    self.flush_block(&mut state, &mut out);
                }
                if state.started {
                    out.push_str("\n\n");
                }
                state.started = false;
                state.fence = None;
            }
        }
        out
    }

    fn line(&self, state: &mut StreamState, line: String, out: &mut String) {
        let trimmed = line.trim_start();
        match state.fence {
            Some(marker) if trimmed.starts_with(marker) => {
                // A closed code block is complete.
                state.fence = None;
                state.block.push(line);
                self.flush_block(state, out);
                return;
            }
            Some(_) => {}
            None => {
                if trimmed.is_empty() {
                    self.flush_block(state, out);
                    return;
                }
                state.fence = ["```", "~~~"]
                    .into_iter()
                    .find(|marker| trimmed.starts_with(marker));
            }
        }
        state.block.push(line);
    }

    fn flush_block(&self, state: &mut StreamState, out: &mut String) {
        if state.block.is_empty() {
            return;
        }
        let block = std::mem::take(&mut state.block).join("\n");
        // Blocks are separated by a blank line, and the first one by a blank
        // line from the progress above it.
        out.push_str(if state.started { "\n\n" } else { "\n" });
        out.push_str(&self.format.apply(&block));
        state.started = true;
    }
}

/// The goal and the state of its acceptance criteria.
pub(super) fn format_goal(goal: &TaskGoal) -> String {
    let mut lines = vec![format!(
        "Goal{}: {}",
        if goal.by_user {
            " (set by the user)"
        } else {
            ""
        },
        goal.objective
    )];
    if goal.acceptance.is_empty() {
        lines.push("  (acceptance criteria not defined yet)".to_string());
    }
    for criterion in &goal.acceptance {
        let status = match criterion.status {
            CriterionStatus::Pending => "pending",
            CriterionStatus::Met => "met",
            CriterionStatus::Blocked => "blocked",
        };
        lines.push(format!(
            "  [{status}] {}: {}{}",
            criterion.id,
            criterion.description,
            criterion
                .evidence
                .as_ref()
                .map(|evidence| format!(" — {evidence}"))
                .unwrap_or_default()
        ));
    }
    lines.join("\n")
}

pub(super) fn format_plan(plan: &TaskPlan) -> String {
    let mut lines = vec![format!(
        "Plan: {:?} (revision {})",
        plan.outcome(),
        plan.revision
    )];
    if let Some(goal) = &plan.goal {
        lines.push(format_goal(goal));
    }
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

/// Progress output on stderr.
struct Progress {
    /// Lines held back while an approval prompt waits for an answer, or
    /// `None` when lines are printed right away.
    held: Option<Vec<String>>,
    /// A status line is on the screen, without a line break after it.
    status_shown: bool,
}

static PROGRESS: Mutex<Progress> = Mutex::new(Progress {
    held: None,
    status_shown: false,
});

/// Stderr is a terminal, where a status line can be redrawn in place.
static STATUS_LINE: LazyLock<bool> = LazyLock::new(|| std::io::stderr().is_terminal());

/// Erases the status line and returns the cursor to the start of the line.
const CLEAR_LINE: &str = "\r\x1b[2K";

macro_rules! progress {
    ($($argument:tt)*) => {
        emit_progress(format!($($argument)*))
    };
}

/// Print a line that stays in the log.
fn emit_progress(line: String) {
    let mut progress = PROGRESS.lock().unwrap_or_else(PoisonError::into_inner);
    match progress.held.as_mut() {
        Some(lines) => lines.push(line),
        None => {
            take_status_line(&mut progress);
            print_progress(&line);
        }
    }
}

/// Show what the agent is doing now. On a terminal the line replaces the
/// previous status instead of adding a line, so that intermediate steps do
/// not fill the screen; elsewhere it is an ordinary progress line.
fn emit_status(line: String) {
    if !*STATUS_LINE {
        emit_progress(line);
        return;
    }
    let mut progress = PROGRESS.lock().unwrap_or_else(PoisonError::into_inner);
    if progress.held.is_some() {
        // Only the latest status matters; the next event shows a new one.
        return;
    }
    let width = terminal_size::terminal_size_of(std::io::stderr())
        .map(|(width, _)| usize::from(width.0))
        .filter(|width| *width >= 20)
        .unwrap_or(80);
    // One column is left free, so that the line never wraps and can be
    // erased in place.
    let line = status_text(&line, width - 1);
    let mut stderr = std::io::stderr().lock();
    if *DIM {
        write!(stderr, "{CLEAR_LINE}{}", line.dim()).ok();
    } else {
        write!(stderr, "{CLEAR_LINE}{line}").ok();
    }
    stderr.flush().ok();
    progress.status_shown = true;
}

/// Erase the status line, if one is shown, before other output.
pub(super) fn clear_status() {
    take_status_line(&mut PROGRESS.lock().unwrap_or_else(PoisonError::into_inner));
}

fn take_status_line(progress: &mut Progress) {
    if std::mem::take(&mut progress.status_shown) {
        let mut stderr = std::io::stderr().lock();
        stderr.write_all(CLEAR_LINE.as_bytes()).ok();
        stderr.flush().ok();
    }
}

/// The first line of `line`, cut to `width` columns.
fn status_text(line: &str, width: usize) -> String {
    let line: String = line
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if UnicodeWidthStr::width(line.as_str()) <= width {
        return line;
    }
    let mut cut = String::new();
    let mut used = 1; // for the ellipsis
    for c in line.chars() {
        used += UnicodeWidthChar::width(c).unwrap_or(0);
        if used > width {
            break;
        }
        cut.push(c);
    }
    cut.push('…');
    cut
}

static DIM: LazyLock<bool> = LazyLock::new(|| {
    std::io::stderr().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
});

/// Print a progress line dimmed on a terminal, so that it stands apart from
/// the answer. `NO_COLOR` turns the styling off.
fn print_progress(line: &str) {
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
        let mut progress = PROGRESS.lock().unwrap_or_else(PoisonError::into_inner);
        take_status_line(&mut progress);
        progress.held.get_or_insert_with(Vec::new);
        Self(())
    }
}

impl Drop for ProgressHold {
    fn drop(&mut self) {
        let held = PROGRESS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .held
            .take();
        for line in held.into_iter().flatten() {
            print_progress(&line);
        }
    }
}

/// The plan in one line: how far it is, and the step being worked on.
fn plan_status(plan: &TaskPlan) -> String {
    let done = plan
        .steps
        .iter()
        .filter(|step| step.status == StepStatus::Completed)
        .count();
    let mut line = format!("[plan] {done}/{} done", plan.steps.len());
    let current = plan
        .steps
        .iter()
        .find(|step| step.status == StepStatus::InProgress)
        .or_else(|| {
            plan.steps
                .iter()
                .find(|step| step.status == StepStatus::Pending)
        });
    if let Some(step) = current {
        line.push_str(&format!(" — {}: {}", step.id, step.description));
    }
    line
}

/// Print an agent event. Intermediate steps go to the status line; what
/// needs attention (failures, refusals, stops) and, with `verbose`,
/// everything stays in the log.
pub(super) fn print_event(event: &AgentEvent, verbose: bool) {
    let status = |line: String| {
        if verbose {
            emit_progress(line)
        } else {
            emit_status(line)
        }
    };
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
        AgentEvent::PlanUpdated { plan, .. } => {
            if verbose {
                progress!("{}", format_plan(plan));
            } else {
                status(plan_status(plan));
            }
        }
        AgentEvent::AssistantProgress { text, streamed, .. } => {
            if !streamed {
                progress!("[agent] {text}")
            }
        }
        AgentEvent::ReasoningSummary { text, .. } => status(format!("[reasoning] {text}")),
        AgentEvent::LocalToolCall {
            name, arguments, ..
        } => {
            if verbose {
                progress!("[tool] {name} {arguments}");
            } else {
                status(format!("[tool] {name}"));
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
            let line = match reason {
                Some(reason) => format!("[approval] {name} -> {decision} ({reason})"),
                None => format!("[approval] {name} -> {decision}"),
            };
            if *approved {
                status(line);
            } else {
                emit_progress(line);
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
                status(format!("[mcp call] {server_label}:{tool_name}"));
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
            let line = match reason {
                Some(reason) => {
                    format!("[mcp approval] {server_label}:{tool_name} -> {decision} ({reason})")
                }
                None => format!("[mcp approval] {server_label}:{tool_name} -> {decision}"),
            };
            if *approved {
                status(line);
            } else {
                emit_progress(line);
            }
        }
        AgentEvent::McpServerUnavailable {
            server_label,
            error,
        } => progress!("[mcp unavailable] {server_label}: {error} (continuing without it)"),
        AgentEvent::ToolSearch { query, results, .. } => {
            status(format!(
                "[tool search] {query} -> {} result(s)",
                results.len()
            ));
        }
        AgentEvent::SubagentStarted { task, model, .. } => {
            let summary = task.lines().next().unwrap_or_default();
            let summary = match summary.char_indices().nth(80) {
                Some((index, _)) => format!("{}…", &summary[..index]),
                None => summary.to_string(),
            };
            status(format!("[subagent] started on {model}: {summary}"));
        }
        AgentEvent::SubagentFinished {
            outcome,
            usage,
            error,
            ..
        } => match (outcome, error) {
            (_, Some(error)) => progress!("[subagent] failed: {error}"),
            (Some(outcome), None) => status(format!(
                "[subagent] finished ({outcome:?}, {} tokens)",
                usage.total_tokens
            )),
            (None, None) => status("[subagent] finished".to_string()),
        },
    }
}

fn print_tool_result(label: &str, output: &serde_json::Value, verbose: bool) {
    if verbose {
        progress!("{label} {output}");
    } else if output.get("error").is_some() || output["isError"] == true {
        progress!("{label} error");
    } else {
        emit_status(format!("{label} received"));
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
            PROGRESS.lock().unwrap().held.as_deref(),
            Some(&["[mcp result] web:search received".to_string()][..])
        );
        drop(hold);
        assert!(PROGRESS.lock().unwrap().held.is_none());
    }

    #[test]
    fn status_text_is_one_line_cut_to_the_width() {
        assert_eq!(
            status_text("\n [reasoning] 調べる\n続き", 80),
            "[reasoning] 調べる"
        );
        assert_eq!(status_text("[tool] a\tb", 80), "[tool] a b");
        // Wide characters count two columns each.
        assert_eq!(status_text("[plan] 日本語の説明", 12), "[plan] 日本…");
        assert_eq!(status_text("abcdef", 6), "abcdef");
        assert_eq!(status_text("abcdefg", 6), "abcde…");
    }

    #[test]
    fn plan_status_names_the_current_step() {
        let step = |id: &str, status| crate::domain::plan::PlanStep {
            id: id.to_string(),
            description: format!("do {id}"),
            status,
            detail: None,
        };
        let plan = TaskPlan {
            revision: 3,
            explanation: None,
            steps: vec![
                step("a", StepStatus::Completed),
                step("b", StepStatus::Pending),
                step("c", StepStatus::InProgress),
            ],
            goal: None,
        };
        assert_eq!(plan_status(&plan), "[plan] 1/3 done — c: do c");
    }

    #[test]
    fn json_output_round_trips_text_and_events() {
        let result = AgentResult {
            usage: UsageSummary::default(),
            stop_reason: StopReason::FinalAnswer,
            streamed: false,
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
    fn streamed_markdown_is_formatted_block_by_block() {
        let stream = AnswerStream::new(
            TextFormat::Terminal {
                width: 40,
                color: false,
            },
            false,
        );
        let mut printed = Vec::new();
        for delta in [
            "## 見",
            "出し\n\n- **太",
            "字**\n",
            "\n```\nlet a",
            " = 1;\n\nlet b = 2;\n",
            "```\n末尾",
        ] {
            printed.push(stream.render(ResponseDelta::Text(delta)));
        }
        // Nothing of a block is printed before the block is complete, and a
        // blank line inside a code fence does not end the block.
        assert_eq!(printed[0], "");
        assert!(printed[1].contains("見出し") && !printed[1].contains('#'));
        assert_eq!(printed[2], "");
        assert!(printed[3].contains("• 太字"));
        assert_eq!(printed[4], "");
        assert!(printed[5].contains("let a = 1;") && printed[5].contains("let b = 2;"));
        let done = stream.render(ResponseDelta::MessageDone);
        assert!(done.contains("末尾") && done.ends_with("\n\n"));
        // The next message starts over.
        assert!(stream
            .render(ResponseDelta::Text("次\n\n"))
            .starts_with("\n次"));
    }

    #[test]
    fn raw_stream_prints_text_as_it_arrives() {
        let stream = AnswerStream::new(TextFormat::Raw, false);
        assert_eq!(stream.render(ResponseDelta::Text("**a")), "\n**a");
        assert_eq!(stream.render(ResponseDelta::Text("b**")), "b**");
        assert_eq!(stream.render(ResponseDelta::MessageDone), "\n\n");
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
