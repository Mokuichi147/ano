//! The page: a header with the session's state and controls, and below it
//! either the form that starts a session or the session itself.

use crate::{api, chat::Chat, dom, setup};
use naui::{
    Align, Button, DialogButtons, DialogResponse, Label, Orientation, Padding, Sizing, Stack,
    Tasks, TextColor, TextStyle, Ui,
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
    stop: Button,
    end: Button,
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
    dom::add_class(&root, "app");

    let header = ui.stack(Orientation::Horizontal)?;
    header.set_spacing(12.0);
    header.set_padding(Padding::symmetric(8.0, 16.0));
    header.set_align(Align::Center);
    dom::add_class(&header, "topbar");
    let brand = ui.label("ano")?;
    brand.set_style(TextStyle::Heading);
    let summary = ui.label("")?;
    summary.set_color(TextColor::Secondary);
    summary.set_sizing(Sizing::fill_width());
    dom::add_class(&summary, "summary");
    let status = ui.label("")?;
    status.set_color(TextColor::Secondary);
    dom::add_class(&status, "status");
    let stop = ui.button("停止")?;
    let end = ui.button("セッションを終了")?;
    header.append(&brand);
    header.append(&summary);
    header.append(&status);
    header.append(&stop);
    header.append(&end);

    let content = ui.stack(Orientation::Vertical)?;
    content.set_sizing(Sizing::fill());
    content.set_align(Align::Fill);
    dom::add_class(&content, "content");
    root.append(&header);
    root.append(&content);
    window.set_child(&root);
    window.show();

    let app = Rc::new(App {
        ui: ui.clone(),
        tasks: ui.tasks(),
        summary,
        status,
        stop: stop.clone(),
        end: end.clone(),
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

    /// Open the server's session, or the form when there is none.
    pub async fn resume(self: Rc<Self>) {
        match api::get("/api/sessions").await {
            Ok(Value::Array(sessions)) if !sessions.is_empty() => {
                self.open_session(&sessions[0]);
            }
            Ok(_) => self.show_setup(None),
            Err(error) => self.show_setup(Some(Message::error(error))),
        }
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
                        app.content.append(&label);
                    }
                    return;
                }
            };
            match setup::build(&app, &options, message) {
                Ok(form) => app.content.append(&form),
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
                dom::set_title(&self.summary, workspace.unwrap_or_default());
                self.set_running(status["running"].as_bool().unwrap_or(false));
                dom::set_visible(&self.end, true);
            }
            None => {
                self.summary.set_text("");
                dom::set_title(&self.summary, "");
                self.status.set_text("");
                dom::set_visible(&self.stop, false);
                dom::set_visible(&self.end, false);
            }
        }
    }

    pub fn set_running(&self, running: bool) {
        self.status
            .set_text(if running { "実行中…" } else { "待機中" });
        dom::set_visible(&self.stop, running);
    }

    /// Show what the model is doing while a turn runs.
    pub fn set_activity(&self, text: &str) {
        self.status.set_text(text);
    }

    fn confirm_end(self: &Rc<Self>) {
        let Some(chat) = self.chat() else { return };
        let Ok(dialog) = self.ui.dialog("セッションを終了しますか？") else {
            return;
        };
        dialog.set_message(if chat.is_running() {
            "実行中のターンを中断して終了します。会話は残りません。"
        } else {
            "会話は残りません。"
        });
        dialog.set_buttons(DialogButtons::new().primary("終了").cancel("キャンセル"));
        let app = Rc::clone(self);
        dialog.on_response(move |response| {
            if response != DialogResponse::Primary {
                return;
            }
            let app = Rc::clone(&app);
            let path = format!("/api/sessions/{}", chat.id());
            app.clone().spawn(async move {
                match api::delete(&path).await {
                    Ok(_) => app.show_setup(None),
                    Err(error) => {
                        if let Some(chat) = app.chat() {
                            chat.notice(
                                &format!("終了できませんでした: {error}"),
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

    pub fn notice(text: &str) -> Self {
        Self {
            text: text.to_string(),
            color: TextColor::Secondary,
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
