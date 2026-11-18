//! The page: a header with the session's state and controls, and below it
//! either the form that starts a session or the session itself.

use crate::{api, chat::Chat, setup};
use naui::{
    web::WebWidgetExt, Align, Button, DialogButtons, DialogResponse, Label, Length, Orientation,
    Padding, ProgressBar, Sizing, Stack, Tasks, TextColor, TextStyle, Ui,
};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    future::Future,
    rc::Rc,
};

pub struct App {
    pub ui: Ui,
    tasks: Tasks,
    summary: Label,
    status: Label,
    /// Moves while a turn runs.
    busy: ProgressBar,
    stop: Button,
    end: Button,
    /// Opens the list of sessions.
    list: Button,
    /// Holds the form or the session.
    content: Stack,
    /// What the server offers for new sessions, once read.
    options: RefCell<Option<Rc<Value>>>,
    chat: RefCell<Option<Rc<Chat>>>,
    /// Counts the views shown in `content`, so a view whose contents arrive
    /// after a later one was asked for is dropped.
    view: Cell<u64>,
}

pub fn build(ui: &Ui) -> naui::Result<()> {
    // The stylesheet lets the window fill the page.
    let window = ui.window("ano", 1600.0, 600.0)?;
    let root = ui.stack(Orientation::Vertical)?;
    root.set_sizing(Sizing::fill());
    root.set_align(Align::Fill);
    root.add_class("app");

    let header = ui.stack(Orientation::Horizontal)?;
    header.set_spacing(12.0);
    header.set_padding(Padding::symmetric(8.0, 16.0));
    header.set_align(Align::Center);
    header.add_class("topbar");
    let brand = ui.label("ano")?;
    brand.set_style(TextStyle::Heading);
    let summary = ui.label("")?;
    summary.set_color(TextColor::Secondary);
    summary.set_sizing(Sizing::fill_width());
    summary.add_class("summary");
    let status = ui.label("")?;
    status.set_color(TextColor::Secondary);
    status.add_class("status");
    let busy = ui.progress_bar()?;
    busy.set_indeterminate(true);
    busy.set_sizing(Sizing::new().width(Length::Fixed(64.0)));
    busy.set_accessible_label(Some("実行中"));
    busy.add_class("busy");
    let stop = ui.button("停止")?;
    stop.set_tooltip(Some(
        "実行中のターンを止めます（入力欄で Esc でも止まります）",
    ));
    let end = ui.button("削除")?;
    end.set_tooltip(Some("このセッションと保存した会話を削除します"));
    let list = ui.button("セッション一覧")?;
    list.set_tooltip(Some("ほかのセッションを開くか、新しく始めます"));
    header.append(&brand);
    header.append(&summary);
    header.append(&status);
    header.append(&busy);
    header.append(&stop);
    header.append(&list);
    header.append(&end);

    let content = ui.stack(Orientation::Vertical)?;
    content.set_sizing(Sizing::fill());
    content.set_align(Align::Fill);
    content.add_class("content");
    root.append(&header);
    root.append(&content);
    window.set_child(&root);
    window.show();

    let app = Rc::new(App {
        ui: ui.clone(),
        tasks: ui.tasks(),
        summary,
        status,
        busy,
        stop: stop.clone(),
        end: end.clone(),
        list: list.clone(),
        content,
        options: RefCell::new(None),
        chat: RefCell::new(None),
        view: Cell::new(0),
    });
    app.show_header(None);
    // The callbacks keep the app alive for the page's lifetime.
    stop.on_click({
        let app = Rc::clone(&app);
        move || {
            if let Some(chat) = app.chat() {
                chat.cancel();
            }
        }
    });
    end.on_click({
        let app = Rc::clone(&app);
        move || app.confirm_end()
    });
    list.on_click({
        let app = Rc::clone(&app);
        move || {
            let app = Rc::clone(&app);
            app.clone().spawn(async move { app.show_sessions().await });
        }
    });
    let started = Rc::clone(&app);
    app.spawn(async move { started.resume().await });
    Ok(())
}

impl App {
    pub fn spawn(&self, future: impl Future<Output = ()> + 'static) {
        self.tasks.spawn(future);
    }

    fn chat(&self) -> Option<Rc<Chat>> {
        self.chat.borrow().clone()
    }

    /// Open the session used last (the one this page had open, when it is
    /// still there), or the form when there is none.
    pub async fn resume(self: Rc<Self>) {
        let current = self.chat().map(|chat| chat.id().to_string());
        match api::get("/api/sessions").await {
            Ok(Value::Array(sessions)) if !sessions.is_empty() => {
                let open = current
                    .and_then(|id| sessions.iter().find(|session| session["id"] == id.as_str()))
                    .unwrap_or(&sessions[0]);
                self.open_session(open);
            }
            Ok(_) => self.show_setup(None),
            Err(error) => self.show_setup(Some(Message::error(error))),
        }
    }

    /// The sessions on the server, most recently used first, to open one or
    /// start another.
    pub async fn show_sessions(self: Rc<Self>) {
        let view = self.next_view();
        let sessions = api::get("/api/sessions").await;
        if self.view.get() != view {
            return;
        }
        self.chat.borrow_mut().take();
        self.show_header(None);
        self.content.clear();
        if let Err(error) = self.build_sessions(sessions) {
            web_sys::console::error_1(&error.to_string().into());
        }
    }

    fn build_sessions(self: &Rc<Self>, sessions: Result<Value, String>) -> naui::Result<()> {
        let ui = &self.ui;
        let page = ui.stack(Orientation::Vertical)?;
        page.set_spacing(8.0);
        page.set_padding(Padding::all(16.0));
        page.set_align(Align::Fill);
        page.add_class("sessions");
        let heading = ui.label("セッション")?;
        heading.set_style(TextStyle::Heading);
        page.append(&heading);
        let new = ui.button("＋ 新しいセッション")?;
        new.on_click({
            let app = Rc::clone(self);
            move || app.show_setup(None)
        });
        page.append(&new);
        match sessions {
            Ok(Value::Array(sessions)) => {
                if sessions.is_empty() {
                    let none = ui.label("まだセッションはありません。")?;
                    none.set_color(TextColor::Secondary);
                    page.append(&none);
                }
                for session in sessions {
                    let title = session["title"]
                        .as_str()
                        .filter(|title| !title.is_empty())
                        .unwrap_or("（まだ会話なし）");
                    let folder = session["workspace"]
                        .as_str()
                        .map_or("（作業フォルダなし）", folder_name);
                    let running = if session["running"].as_bool() == Some(true) {
                        "  ·  実行中"
                    } else {
                        ""
                    };
                    let button = ui.button(&format!("{title}　—　{folder}{running}"))?;
                    button.set_tooltip(session["model"].as_str());
                    button.add_class("session-item");
                    button.on_click({
                        let app = Rc::clone(self);
                        move || app.open_session(&session)
                    });
                    page.append(&button);
                }
            }
            Ok(_) => {}
            Err(error) => {
                let label = ui.label(&error)?;
                label.set_color(TextColor::Danger);
                label.set_selectable(true);
                page.append(&label);
            }
        }
        self.content.append(&page);
        new.request_focus();
        Ok(())
    }

    /// Show the form that starts a session, with `message` above its button.
    pub fn show_setup(self: &Rc<Self>, message: Option<Message>) {
        self.chat.borrow_mut().take();
        self.show_header(None);
        self.content.clear();
        let view = self.next_view();
        let app = Rc::clone(self);
        self.spawn(async move {
            let options = app.options().await;
            if app.view.get() != view {
                return;
            }
            let options = match options {
                Ok(options) => options,
                Err(error) => {
                    if let Ok(label) = app.ui.label(&error) {
                        label.set_color(TextColor::Danger);
                        label.set_selectable(true);
                        app.content.append(&label);
                    }
                    return;
                }
            };
            match setup::build(&app, &options, message) {
                Ok(form) => {
                    app.content.append(&form);
                    // The first field: the working folder.
                    form.request_focus();
                }
                Err(error) => web_sys::console::error_1(&error.to_string().into()),
            }
        });
    }

    fn next_view(&self) -> u64 {
        self.view.set(self.view.get() + 1);
        self.view.get()
    }

    async fn options(&self) -> Result<Rc<Value>, String> {
        if let Some(options) = self.options.borrow().clone() {
            return Ok(options);
        }
        let options = Rc::new(api::get("/api/options").await?);
        *self.options.borrow_mut() = Some(Rc::clone(&options));
        Ok(options)
    }

    /// Show session `status` (as the server describes it) and follow it.
    pub fn open_session(self: &Rc<Self>, status: &Value) {
        self.next_view();
        self.content.clear();
        match Chat::open(self, status) {
            Ok(chat) => {
                self.content.append(chat.view());
                chat.focus();
                self.show_header(Some(status));
                *self.chat.borrow_mut() = Some(chat);
            }
            Err(error) => self.show_setup(Some(Message::error(error.to_string()))),
        }
    }

    /// The header for session `status`, or for no session.
    fn show_header(&self, status: Option<&Value>) {
        match status {
            Some(status) => {
                let workspace = status["workspace"].as_str();
                let model = status["model"].as_str().unwrap_or_default();
                let folder = workspace.map_or("（作業フォルダなし）", folder_name);
                self.summary.set_text(&format!("{folder}  ·  {model}"));
                self.summary.set_tooltip(workspace);
                self.set_running(status["running"].as_bool().unwrap_or(false));
                self.list.set_visible(true);
            }
            None => {
                self.summary.set_text("");
                self.summary.set_tooltip(None);
                self.status.set_text("");
                self.busy.set_visible(false);
                self.stop.set_visible(false);
                self.end.set_visible(false);
                self.list.set_visible(true);
            }
        }
    }

    pub fn set_running(&self, running: bool) {
        self.status
            .set_text(if running { "実行中…" } else { "待機中" });
        self.busy.set_visible(running);
        self.stop.set_visible(running);
        // A phone's header has no room for both; stop the turn first.
        self.end.set_visible(!running);
    }

    /// Show what the model is doing while a turn runs.
    pub fn set_activity(&self, text: &str) {
        self.status.set_text(text);
    }

    fn confirm_end(self: &Rc<Self>) {
        let Some(chat) = self.chat() else { return };
        let Ok(dialog) = self.ui.dialog("このセッションを削除しますか？") else {
            return;
        };
        dialog.set_message(if chat.is_running() {
            "実行中のターンを中断して削除します。保存した会話も消え、元に戻せません。"
        } else {
            "保存した会話も消え、元に戻せません。ほかのセッションを開くには「セッション一覧」を使います。"
        });
        dialog.set_buttons(DialogButtons::new().primary("削除").cancel("キャンセル"));
        let app = Rc::clone(self);
        dialog.on_response(move |response| {
            if response != DialogResponse::Primary {
                return;
            }
            let app = Rc::clone(&app);
            let path = format!("/api/sessions/{}", chat.id());
            app.clone().spawn(async move {
                match api::delete(&path).await {
                    Ok(_) => app.clone().spawn(async move { app.show_sessions().await }),
                    Err(error) => {
                        if let Some(chat) = app.chat() {
                            chat.notice(
                                &format!("削除できませんでした: {error}"),
                                TextColor::Danger,
                            );
                        }
                    }
                }
            });
        });
        dialog.open();
    }
}

/// A line above the form's button.
pub struct Message {
    pub text: String,
    pub color: TextColor,
}

impl Message {
    pub fn error(text: String) -> Self {
        Self {
            text,
            color: TextColor::Danger,
        }
    }
}

/// The last part of `path`, the folder's own name.
fn folder_name(path: &str) -> &str {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
}
