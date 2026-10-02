//! An open session: the timeline of its events, the composer, and its plan,
//! usage, and settings beside them.

use crate::{
    api,
    app::{App, Message},
    dom,
    events::EventStream,
    timeline::{self, one_line, pretty},
};
use naui::{
    Align, Button, GridCell, Label, Orientation, Padding, Scroll, ScrollPolicy, Sizing, Stack,
    TextArea, TextColor, TextStyle, Track, Widget,
};
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
};

pub struct Chat {
    app: Weak<App>,
    id: String,
    view: Stack,
    scroll: Scroll,
    timeline: Stack,
    prompt: TextArea,
    send: Button,
    plan: Stack,
    usage: Label,
    /// The message being generated.
    live: RefCell<Option<Live>>,
    /// Approval requests that wait for an answer, by id.
    approvals: RefCell<HashMap<String, Approval>>,
    running: Cell<bool>,
    stream: RefCell<Option<EventStream>>,
    /// Sends with Enter while the view lives.
    enter: RefCell<Option<dom::Listener>>,
}

struct Live {
    bubble: Stack,
    label: Label,
    text: String,
}

struct Approval {
    card: Stack,
    buttons: Stack,
    allow: Button,
    deny: Button,
}

impl Chat {
    /// Build the view of session `status` and follow its events.
    pub fn open(app: &Rc<App>, status: &Value) -> naui::Result<Rc<Self>> {
        let ui = &app.ui;
        let id = status["id"].as_str().unwrap_or_default().to_string();

        let timeline = ui.stack(Orientation::Vertical)?;
        timeline.set_spacing(10.0);
        timeline.set_padding(Padding::symmetric(20.0, 24.0));
        timeline.set_sizing(Sizing::fill_width());
        timeline.set_align(Align::Fill);
        dom::add_class(&timeline, "timeline");
        let scroll = ui.scroll()?;
        scroll.set_policy(ScrollPolicy::Never, ScrollPolicy::Auto);
        scroll.set_sizing(Sizing::fill());
        scroll.set_child(&timeline);

        let prompt = ui.text_area("")?;
        prompt.set_placeholder("メッセージ（Enter で送信、Shift+Enter で改行）");
        prompt.set_sizing(Sizing::fill_width().height(naui::Length::Fixed(72.0)));
        let send = ui.button("送信")?;
        let composer = ui.stack(Orientation::Horizontal)?;
        composer.set_spacing(10.0);
        composer.set_padding(Padding::symmetric(12.0, 24.0));
        composer.set_align(Align::End);
        dom::add_class(&composer, "composer");
        composer.append(&prompt);
        composer.append(&send);

        let main = ui.stack(Orientation::Vertical)?;
        main.set_sizing(Sizing::fill());
        main.set_align(Align::Fill);
        dom::add_class(&main, "main");
        main.append(&scroll);
        main.append(&composer);

        let plan = ui.stack(Orientation::Vertical)?;
        plan.set_spacing(4.0);
        plan.set_align(Align::Fill);
        let usage = ui.label("—")?;
        usage.set_wrap(true);
        let side = ui.stack(Orientation::Vertical)?;
        side.set_spacing(8.0);
        side.set_padding(Padding::symmetric(14.0, 16.0));
        side.set_sizing(Sizing::fill_width());
        side.set_align(Align::Fill);
        side.append(&heading(app, "計画")?);
        side.append(&plan);
        side.append(&heading(app, "使用量")?);
        side.append(&usage);
        side.append(&heading(app, "セッション")?);
        side.append(&info(app, status)?);
        let side_scroll = ui.scroll()?;
        side_scroll.set_policy(ScrollPolicy::Never, ScrollPolicy::Auto);
        side_scroll.set_sizing(Sizing::fill_height().width(naui::Length::Fixed(300.0)));
        side_scroll.set_child(&side);
        dom::add_class(&side_scroll, "side");

        let view = ui.stack(Orientation::Horizontal)?;
        view.set_sizing(Sizing::fill());
        dom::add_class(&view, "chat");
        view.append(&main);
        view.append(&side_scroll);

        let chat = Rc::new(Self {
            app: Rc::downgrade(app),
            id,
            view,
            scroll,
            timeline,
            prompt: prompt.clone(),
            send: send.clone(),
            plan,
            usage,
            live: RefCell::new(None),
            approvals: RefCell::new(HashMap::new()),
            running: Cell::new(false),
            stream: RefCell::new(None),
            enter: RefCell::new(None),
        });
        chat.render_plan(&status["plan"]);
        chat.usage.set_text(&timeline::usage(&status["usage"]));
        chat.set_running(status["running"] == true);

        let weak = Rc::downgrade(&chat);
        send.on_click({
            let weak = weak.clone();
            move || {
                if let Some(chat) = weak.upgrade() {
                    chat.submit();
                }
            }
        });
        *chat.enter.borrow_mut() = dom::on_enter(&prompt, {
            let weak = weak.clone();
            move || {
                if let Some(chat) = weak.upgrade() {
                    chat.submit();
                }
            }
        });
        let stream = EventStream::open(
            &chat.id,
            {
                let weak = weak.clone();
                move |event| {
                    if let Some(chat) = weak.upgrade() {
                        chat.handle(&event);
                    }
                }
            },
            {
                let app = chat.app.clone();
                move || {
                    // The session or the server is gone: show what is there now.
                    if let Some(app) = app.upgrade() {
                        let resumed = Rc::clone(&app);
                        app.spawn(async move { resumed.resume().await });
                    }
                }
            },
        );
        *chat.stream.borrow_mut() = stream;
        dom::focus(&prompt);
        Ok(chat)
    }

    pub fn view(&self) -> &Stack {
        &self.view
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn is_running(&self) -> bool {
        self.running.get()
    }

    fn set_running(&self, running: bool) {
        self.running.set(running);
        self.send.set_enabled(!running);
        if let Some(app) = self.app.upgrade() {
            app.set_running(running);
        }
    }

    /// Send the composer's text as the next turn.
    fn submit(self: &Rc<Self>) {
        let text = self.prompt.text();
        if text.trim().is_empty() || self.running.get() {
            return;
        }
        let Some(app) = self.app.upgrade() else {
            return;
        };
        self.send.set_enabled(false);
        let chat = Rc::clone(self);
        app.spawn(async move {
            let path = format!("/api/sessions/{}/messages", chat.id);
            match api::post(&path, &json!({ "text": text })).await {
                // Keep what was typed while the message was being sent.
                Ok(_) if chat.prompt.text() == text => chat.prompt.set_text(""),
                Ok(_) => {}
                Err(error) => {
                    chat.notice(&format!("送信できませんでした: {error}"), TextColor::Danger);
                    chat.send.set_enabled(!chat.running.get());
                }
            }
        });
    }

    pub fn cancel(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let chat = Rc::clone(self);
        app.spawn(async move {
            let path = format!("/api/sessions/{}/cancel", chat.id);
            if let Err(error) = api::post(&path, &Value::Null).await {
                chat.notice(&format!("停止できませんでした: {error}"), TextColor::Danger);
            }
        });
    }

    /// Add `widget` to the timeline, keeping the end in view when the reader
    /// was there.
    fn append(&self, widget: &dyn Widget) {
        let following = dom::near_end(&self.scroll);
        self.timeline.append(widget);
        if following {
            dom::scroll_to_end(&self.scroll);
        }
    }

    /// A line of text in the timeline.
    pub fn notice(&self, text: &str, color: TextColor) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        if let Ok(label) = app.ui.label(text) {
            label.set_wrap(true);
            label.set_color(color);
            dom::add_class(&label, "line");
            self.append(&label);
        }
    }

    fn handle(self: &Rc<Self>, event: &Value) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let text = event["text"].as_str().unwrap_or_default();
        match event["type"].as_str().unwrap_or_default() {
            "user_message" => {
                self.live.borrow_mut().take();
                if let Ok(bubble) = bubble(&app, "user") {
                    if let Ok(label) = app.ui.label(text) {
                        label.set_wrap(true);
                        bubble.append(&label);
                    }
                    self.append(&bubble);
                }
            }
            "turn_started" => self.set_running(true),
            // The events that follow are all those the server kept.
            "reset" => {
                self.timeline.clear();
                self.live.borrow_mut().take();
                self.approvals.borrow_mut().clear();
                if event["truncated"] == true {
                    self.notice(
                        "古いイベントは保持の上限を超えたため表示されません。",
                        TextColor::Secondary,
                    );
                }
            }
            "delta" => {
                app.set_activity("実行中…");
                self.stream_text(&app, text, true);
            }
            "partial" => self.stream_text(&app, text, false),
            "reasoning" => app.set_activity("考え中…"),
            "message" => self.show_message(&app, event["html"].as_str().unwrap_or_default()),
            "agent" => self.agent_event(&app, &event["event"]),
            "approval_requested" => self.ask_approval(&app, event),
            "approval_resolved" => self.resolve_approval(
                &app,
                event["id"].as_str().unwrap_or_default(),
                event["approved"] == true,
            ),
            "turn_finished" => self.finish_turn(event),
            "closed" => {
                // Replacing the view drops this stream, which must not
                // happen inside its own callback.
                let app = Rc::clone(&app);
                app.clone().spawn(async move {
                    app.show_setup(Some(Message::notice("セッションを終了しました。")))
                });
            }
            _ => {}
        }
    }

    /// Show streamed `text`, added to or replacing what came so far.
    fn stream_text(&self, app: &App, text: &str, append: bool) {
        let mut live = self.live.borrow_mut();
        if live.is_none() {
            let (Ok(bubble), Ok(label)) = (bubble(app, "assistant streaming"), app.ui.label(""))
            else {
                return;
            };
            label.set_wrap(true);
            bubble.append(&label);
            self.append(&bubble);
            *live = Some(Live {
                bubble,
                label,
                text: String::new(),
            });
        }
        let Some(live) = live.as_mut() else { return };
        if !append {
            live.text.clear();
        }
        live.text.push_str(text);
        let following = dom::near_end(&self.scroll);
        live.label.set_text(&live.text);
        if following {
            dom::scroll_to_end(&self.scroll);
        }
    }

    /// A complete message, as HTML; it takes the place of the streamed text.
    fn show_message(&self, app: &App, html: &str) {
        let following = dom::near_end(&self.scroll);
        let bubble = match self.live.borrow_mut().take() {
            Some(live) => {
                live.bubble.clear();
                dom::remove_class(&live.bubble, "streaming");
                live.bubble
            }
            None => {
                let Ok(bubble) = bubble(app, "assistant") else {
                    return;
                };
                self.timeline.append(&bubble);
                bubble
            }
        };
        dom::add_class(&bubble, "markdown");
        dom::set_html(&bubble, html);
        if following {
            dom::scroll_to_end(&self.scroll);
        }
    }

    /// A collapsed row whose details show `body`.
    fn detail(&self, app: &App, title: &str, body: &str) {
        let Ok(expander) = app.ui.expander(title) else {
            return;
        };
        expander.set_sizing(Sizing::fill_width());
        dom::add_class(&expander, "event");
        if let Ok(label) = app.ui.label(body) {
            label.set_wrap(true);
            dom::add_class(&label, "code");
            expander.set_child(&label);
        }
        self.append(&expander);
    }

    fn agent_event(&self, app: &App, event: &Value) {
        let field = |name: &str| event[name].as_str().unwrap_or_default();
        let target = || match event["server_label"].as_str() {
            Some(server) => format!("{server}:{}", field("tool_name")),
            None => field("name").to_string(),
        };
        match field("type") {
            "plan_updated" => self.render_plan(&event["plan"]),
            "usage_updated" | "execution_stopped" => {}
            "reasoning_summary" => self.detail(
                app,
                &format!("推論の要約  {}", one_line(&event["text"], 80)),
                &pretty(&event["text"]),
            ),
            "local_tool_call" | "mcp_tool_call" => self.detail(
                app,
                &format!("{}  {}", target(), one_line(&event["arguments"], 120)),
                &pretty(&event["arguments"]),
            ),
            "local_tool_result" | "mcp_tool_result" => self.detail(
                app,
                &format!("↳ {}  {}", target(), one_line(&event["output"], 120)),
                &pretty(&event["output"]),
            ),
            "local_tool_blocked" | "mcp_tool_blocked" => self.notice(
                &format!("{} は許可されていないため実行しませんでした", target()),
                TextColor::Warning,
            ),
            "local_tool_approval" | "mcp_approval" => {
                let approved = event["approved"] == true;
                let reason = event["reason"]
                    .as_str()
                    .map(|reason| format!("（{reason}）"))
                    .unwrap_or_default();
                self.notice(
                    &format!(
                        "承認 {}: {}{reason}",
                        target(),
                        if approved { "許可" } else { "拒否" }
                    ),
                    if approved {
                        TextColor::Secondary
                    } else {
                        TextColor::Warning
                    },
                );
            }
            "mcp_server_unavailable" => self.notice(
                &format!(
                    "MCP サーバー {} に接続できません: {}",
                    field("server_label"),
                    one_line(&event["error"], 300)
                ),
                TextColor::Warning,
            ),
            "tool_search" => self.detail(
                app,
                &format!("tool_search  {}", one_line(&event["query"], 120)),
                &pretty(&event["results"]),
            ),
            "subagent_started" => self.detail(
                app,
                &format!(
                    "サブエージェント開始 {}  {}",
                    field("model"),
                    one_line(&event["task"], 120)
                ),
                &pretty(&event["task"]),
            ),
            "subagent_finished" => match event["error"].as_str() {
                Some(error) => self.notice(
                    &format!(
                        "サブエージェント終了: 失敗（{}）",
                        one_line(&json!(error), 200)
                    ),
                    TextColor::Danger,
                ),
                None => self.notice(
                    &format!(
                        "サブエージェント終了: {}",
                        timeline::outcome(field("outcome"))
                    ),
                    TextColor::Secondary,
                ),
            },
            "context_compacted" => self.notice(
                &format!(
                    "履歴を圧縮しました（{} → {} 件）",
                    event["record"]["before_items"], event["record"]["after_items"]
                ),
                TextColor::Secondary,
            ),
            other => self.detail(app, other, &pretty(event)),
        }
    }

    fn ask_approval(self: &Rc<Self>, app: &App, event: &Value) {
        let id = event["id"].as_str().unwrap_or_default().to_string();
        let build = || -> naui::Result<Approval> {
            let card = app.ui.stack(Orientation::Vertical)?;
            card.set_spacing(6.0);
            card.set_padding(Padding::symmetric(10.0, 14.0));
            card.set_sizing(Sizing::fill_width());
            card.set_align(Align::Start);
            dom::add_class(&card, "approval");
            let heading = app.ui.label(if event["mcp"] == true {
                "MCP の呼び出しの承認"
            } else {
                "tool の実行の承認"
            })?;
            heading.set_style(TextStyle::Heading);
            card.append(&heading);
            card.append(&app.ui.label(&format!(
                "対象: {}",
                event["target"].as_str().unwrap_or_default()
            ))?);
            if let Some(review) = event["review"].as_str() {
                let label = app.ui.label(&format!("自動判定: {review}"))?;
                label.set_wrap(true);
                label.set_color(TextColor::Secondary);
                card.append(&label);
            }
            if event["arguments"]["truncated"] == true {
                let label = app.ui.label("引数が長いため、先頭だけを表示しています。")?;
                label.set_color(TextColor::Warning);
                card.append(&label);
            }
            let arguments = app.ui.label(&pretty(&event["arguments"]))?;
            arguments.set_wrap(true);
            dom::add_class(&arguments, "code");
            card.append(&arguments);
            let buttons = app.ui.stack(Orientation::Horizontal)?;
            buttons.set_spacing(8.0);
            let allow = app.ui.button("許可")?;
            let deny = app.ui.button("拒否")?;
            for (button, approved) in [(&allow, true), (&deny, false)] {
                let weak = Rc::downgrade(self);
                let id = id.clone();
                let (allow, deny) = (allow.clone(), deny.clone());
                button.on_click(move || {
                    let Some(chat) = weak.upgrade() else { return };
                    allow.set_enabled(false);
                    deny.set_enabled(false);
                    chat.answer(&id, approved);
                });
            }
            buttons.append(&allow);
            buttons.append(&deny);
            card.append(&buttons);
            dom::focus(&allow);
            Ok(Approval {
                card,
                buttons,
                allow,
                deny,
            })
        };
        if let Ok(approval) = build() {
            self.append(&approval.card);
            self.approvals.borrow_mut().insert(id, approval);
        }
    }

    fn answer(self: &Rc<Self>, id: &str, approved: bool) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let chat = Rc::clone(self);
        let path = format!("/api/sessions/{}/approvals/{id}", self.id);
        let id = id.to_string();
        app.spawn(async move {
            if let Err(error) = api::post(&path, &json!({ "approved": approved })).await {
                // Still waiting (e.g. the request did not arrive): answer again.
                if let Some(approval) = chat.approvals.borrow().get(&id) {
                    approval.allow.set_enabled(true);
                    approval.deny.set_enabled(true);
                }
                chat.notice(
                    &format!("承認を送れませんでした: {error}"),
                    TextColor::Warning,
                );
            }
        });
    }

    fn resolve_approval(&self, app: &App, id: &str, approved: bool) {
        let Some(approval) = self.approvals.borrow_mut().remove(id) else {
            return;
        };
        dom::add_class(&approval.card, "resolved");
        let card = &approval.card;
        if let Some(index) = (0..card.len()).last() {
            card.remove(index);
        }
        drop(approval.buttons);
        if let Ok(result) = app.ui.label(if approved {
            "許可しました"
        } else {
            "拒否しました"
        }) {
            result.set_style(TextStyle::Heading);
            result.set_color(if approved {
                TextColor::Success
            } else {
                TextColor::Danger
            });
            card.append(&result);
        }
    }

    fn finish_turn(self: &Rc<Self>, event: &Value) {
        if let Some(live) = self.live.borrow_mut().take() {
            dom::remove_class(&live.bubble, "streaming");
        }
        self.set_running(false);
        self.usage.set_text(&timeline::usage(&event["usage"]));
        if event["cancelled"] == true {
            self.notice(
                "中断しました。完了した操作は元に戻りません。",
                TextColor::Warning,
            );
        }
        if let Some(error) = event["error"].as_str() {
            self.notice(&format!("エラー: {error}"), TextColor::Danger);
        }
        let mut notes = Vec::new();
        if let Some(reason) = event["stop_reason"]
            .as_str()
            .and_then(timeline::stop_reason)
        {
            notes.push(reason.to_string());
        }
        if let Some(outcome) = event["outcome"]
            .as_str()
            .filter(|outcome| *outcome != "completed")
        {
            notes.push(format!("計画: {}", timeline::outcome(outcome)));
        }
        if !notes.is_empty() {
            self.notice(&notes.join("・"), TextColor::Warning);
        }
        // The plan at the end of the turn, as the conversation keeps it.
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let chat = Rc::clone(self);
        app.spawn(async move {
            if let Ok(status) = api::get(&format!("/api/sessions/{}", chat.id)).await {
                chat.render_plan(&status["plan"]);
            }
        });
    }

    fn render_plan(&self, plan: &Value) {
        self.plan.clear();
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let steps = plan["steps"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let goal = &plan["goal"];
        if goal.is_null() && steps.is_empty() {
            if let Ok(label) = app.ui.label("まだありません") {
                label.set_color(TextColor::Secondary);
                self.plan.append(&label);
            }
            return;
        }
        let mut items: Vec<(String, &str)> = Vec::new();
        if let Some(objective) = goal["objective"].as_str() {
            items.push((format!("ゴール: {objective}"), ""));
            for criterion in goal["acceptance"].as_array().into_iter().flatten() {
                let status = criterion["status"].as_str().unwrap_or_default();
                items.push((
                    format!(
                        "{} {}",
                        timeline::mark(status),
                        criterion["description"].as_str().unwrap_or_default()
                    ),
                    status,
                ));
            }
        }
        for step in steps {
            let status = step["status"].as_str().unwrap_or_default();
            items.push((
                format!(
                    "{} {}",
                    timeline::mark(status),
                    step["description"].as_str().unwrap_or_default()
                ),
                status,
            ));
        }
        for (text, status) in items {
            let Ok(label) = app.ui.label(&text) else {
                continue;
            };
            label.set_wrap(true);
            match status {
                "" => label.set_style(TextStyle::Heading),
                "in_progress" => label.set_color(TextColor::Accent),
                "completed" | "met" => label.set_color(TextColor::Secondary),
                "blocked" => label.set_color(TextColor::Danger),
                _ => {}
            }
            self.plan.append(&label);
        }
    }
}

fn heading(app: &App, text: &str) -> naui::Result<Label> {
    let label = app.ui.label(text)?;
    label.set_style(TextStyle::Heading);
    label.set_color(TextColor::Secondary);
    Ok(label)
}

fn bubble(app: &App, classes: &str) -> naui::Result<Stack> {
    let bubble = app.ui.stack(Orientation::Vertical)?;
    bubble.set_padding(Padding::symmetric(10.0, 14.0));
    bubble.set_align(Align::Start);
    dom::add_class(&bubble, &format!("bubble {classes}"));
    Ok(bubble)
}

/// The session's settings as name and value rows.
fn info(app: &App, status: &Value) -> naui::Result<naui::Grid> {
    let text = |field: &str| status[field].as_str().unwrap_or("—").to_string();
    let permissions = [("allow_writes", "書き込み"), ("allow_exec", "コマンド")]
        .iter()
        .filter(|(field, _)| status[*field] == true)
        .map(|(_, name)| *name)
        .collect::<Vec<_>>()
        .join("・");
    let rows = [
        ("作業フォルダ", text("workspace")),
        ("環境", text("environment")),
        ("モデル", text("model")),
        ("接続先", text("endpoint")),
        (
            "権限",
            if permissions.is_empty() {
                "読み取りのみ".to_string()
            } else {
                permissions
            },
        ),
        (
            "承認",
            timeline::approval_mode(&text("approval_mode")).to_string(),
        ),
        ("ユーザー", text("user")),
    ];
    let grid = app.ui.grid()?;
    grid.set_spacing(10.0, 4.0);
    dom::add_class(&grid, "info");
    grid.set_column_track(1, Track::FILL);
    grid.set_sizing(Sizing::fill_width());
    for (row, (name, value)) in rows.into_iter().enumerate() {
        let name = app.ui.label(name)?;
        name.set_color(TextColor::Secondary);
        let value = app.ui.label(&value)?;
        value.set_wrap(true);
        dom::add_class(&value, "value");
        grid.attach(&name, GridCell::new(0, row));
        grid.attach(&value, GridCell::new(1, row));
    }
    Ok(grid)
}
