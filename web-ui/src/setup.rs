//! The form that starts a session: its working folder, environment, preset,
//! permissions, and approval mode.

use crate::{
    api,
    app::{App, Message},
    dom,
};
use naui::{
    Align, Checkbox, ComboBox, GridCell, Label, Orientation, Padding, Sizing, Stack, TextColor,
    TextInput, TextStyle, Track,
};
use serde_json::{json, Map, Value};
use std::{cell::Cell, rc::Rc};

const APPROVAL_MODES: [(&str, &str); 4] = [
    ("ask", "ask — ブラウザで確認する"),
    ("auto", "auto — 判定モデルが決め、迷うものだけ確認する"),
    ("allow", "allow — すべて許可する"),
    ("deny", "deny — すべて拒否する"),
];

const WORKSPACE_NOTE: &str =
    "~ はホームディレクトリ。エージェントのファイル操作とコマンドはこのフォルダの中で行われます";

struct Form {
    options: Rc<Value>,
    workspace: TextInput,
    /// The workspace is the environment's and cannot be edited.
    workspace_fixed: Cell<bool>,
    note: Label,
    environment: ComboBox,
    preset: ComboBox,
    writes: Checkbox,
    exec: Checkbox,
    web: Checkbox,
    approval: ComboBox,
    error: Label,
}

pub fn build(app: &Rc<App>, options: &Rc<Value>, message: Option<Message>) -> naui::Result<Stack> {
    let ui = &app.ui;
    let card = ui.stack(Orientation::Vertical)?;
    card.set_spacing(10.0);
    card.set_padding(Padding::all(24.0));
    card.set_sizing(Sizing::fill_width().max_width(640.0));
    card.set_align(Align::Fill);
    dom::add_class(&card, "card");

    let title = ui.label("新しいセッション")?;
    title.set_style(TextStyle::Title);
    card.append(&title);

    let workspace = ui.text_input(options["default_workspace"].as_str().unwrap_or_default())?;
    workspace.set_sizing(Sizing::fill_width());
    let note = ui.label(WORKSPACE_NOTE)?;
    note.set_color(TextColor::Secondary);
    note.set_wrap(true);
    card.append(&caption(app, "作業フォルダ")?);
    card.append(&workspace);
    card.append(&note);

    let environment = ui.combo_box()?;
    let mut environments = vec!["なし（ここで権限を選ぶ）".to_string()];
    environments.extend(names(options, "environments").map(str::to_string));
    environment.set_items(&environments.iter().map(String::as_str).collect::<Vec<_>>());
    environment.set_selected(0);
    environment.set_sizing(Sizing::fill_width());
    let preset = ui.combo_box()?;
    let mut presets = vec![format!(
        "default — {}",
        options["default_preset"]["summary"]
            .as_str()
            .unwrap_or_default()
    )];
    for preset in options["presets"].as_array().into_iter().flatten() {
        let detail = preset["description"]
            .as_str()
            .or(preset["summary"].as_str())
            .unwrap_or_default();
        presets.push(format!(
            "{} — {detail}",
            preset["name"].as_str().unwrap_or_default()
        ));
    }
    preset.set_items(&presets.iter().map(String::as_str).collect::<Vec<_>>());
    preset.set_selected(0);
    preset.set_sizing(Sizing::fill_width());
    let choices = ui.grid()?;
    choices.set_spacing(12.0, 4.0);
    choices.set_column_track(0, Track::FILL);
    choices.set_column_track(1, Track::FILL);
    choices.set_sizing(Sizing::fill_width());
    choices.attach(&caption(app, "環境")?, GridCell::new(0, 0));
    choices.attach(&caption(app, "プリセット")?, GridCell::new(1, 0));
    choices.attach(&environment, GridCell::new(0, 1));
    choices.attach(&preset, GridCell::new(1, 1));
    card.append(&choices);

    card.append(&caption(app, "権限")?);
    let writes = ui.checkbox("ファイルの書き込み・編集")?;
    let exec = ui.checkbox("コマンドの実行（実行ごとに承認）")?;
    let web = ui.checkbox("Web ページの取得（取得ごとに承認）")?;
    card.append(&writes);
    card.append(&exec);
    card.append(&web);

    card.append(&caption(app, "承認モード")?);
    let approval = ui.combo_box()?;
    approval.set_items(&APPROVAL_MODES.map(|(_, label)| label));
    approval.set_sizing(Sizing::fill_width());
    card.append(&approval);

    let error = ui.label("")?;
    error.set_wrap(true);
    card.append(&error);
    let start = ui.button("開始")?;
    card.append(&start);

    let form = Rc::new(Form {
        options: Rc::clone(options),
        workspace,
        workspace_fixed: Cell::new(false),
        note,
        environment: environment.clone(),
        preset,
        writes,
        exec,
        web,
        approval,
        error,
    });
    form.select_approval(options["default_approval_mode"].as_str().unwrap_or("ask"));
    form.show_message(message);
    environment.on_select({
        let form = Rc::clone(&form);
        move |_| form.follow_environment()
    });
    start.on_click({
        let app = Rc::clone(app);
        let start = start.clone();
        move || {
            let body = form.request();
            let app = Rc::clone(&app);
            let form = Rc::clone(&form);
            let start = start.clone();
            start.set_enabled(false);
            app.clone().spawn(async move {
                match api::post("/api/sessions", &body).await {
                    Ok(status) => app.open_session(&status),
                    Err(error) => {
                        form.show_message(Some(Message::error(error)));
                        start.set_enabled(true);
                    }
                }
            });
        }
    });

    // Center the card in the page.
    let page = ui.stack(Orientation::Vertical)?;
    page.set_align(Align::Center);
    page.set_padding(Padding::all(32.0));
    page.set_sizing(Sizing::fill());
    dom::add_class(&page, "setup");
    page.append(&card);
    Ok(page)
}

fn caption(app: &App, text: &str) -> naui::Result<Label> {
    let label = app.ui.label(text)?;
    label.set_style(TextStyle::Heading);
    label.set_color(TextColor::Secondary);
    Ok(label)
}

fn names<'a>(options: &'a Value, field: &str) -> impl Iterator<Item = &'a str> {
    options[field]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["name"].as_str())
}

impl Form {
    /// The chosen environment, or `None` for permissions chosen here.
    fn environment(&self) -> Option<&Value> {
        let index = self.environment.selected().filter(|index| *index > 0)?;
        self.options["environments"].get(index - 1)
    }

    fn select_approval(&self, mode: &str) {
        if let Some(index) = APPROVAL_MODES.iter().position(|(name, _)| *name == mode) {
            self.approval.set_selected(index);
        }
    }

    /// An environment brings its permissions, approval mode, and maybe its
    /// workspace; choosing none returns to the defaults.
    fn follow_environment(&self) {
        let environment = self.environment();
        let fixed = environment.is_some();
        for checkbox in [&self.writes, &self.exec, &self.web] {
            checkbox.set_enabled(!fixed);
        }
        self.approval.set_enabled(!fixed);
        let default_workspace = self.options["default_workspace"]
            .as_str()
            .unwrap_or_default();
        match environment {
            Some(environment) => {
                self.writes.set_checked(environment["allow_writes"] == true);
                self.exec.set_checked(environment["allow_exec"] == true);
                self.web.set_checked(environment["allow_web"] == true);
                self.select_approval(environment["approval_mode"].as_str().unwrap_or("ask"));
                match environment["workspace"].as_str() {
                    Some(workspace) => {
                        self.workspace.set_text(workspace);
                        self.workspace.set_enabled(false);
                        self.workspace_fixed.set(true);
                        self.note.set_text(&format!(
                            "環境 {} の作業フォルダを使います",
                            environment["name"].as_str().unwrap_or_default()
                        ));
                    }
                    None => self.free_workspace(default_workspace),
                }
            }
            None => {
                for checkbox in [&self.writes, &self.exec, &self.web] {
                    checkbox.set_checked(false);
                }
                self.select_approval(
                    self.options["default_approval_mode"]
                        .as_str()
                        .unwrap_or("ask"),
                );
                self.free_workspace(default_workspace);
            }
        }
    }

    fn free_workspace(&self, default_workspace: &str) {
        if self.workspace_fixed.replace(false) {
            self.workspace.set_text(default_workspace);
        }
        self.workspace.set_enabled(true);
        self.note.set_text(WORKSPACE_NOTE);
    }

    /// The body of `POST /api/sessions`.
    fn request(&self) -> Value {
        let mut body = Map::new();
        let environment = self.environment();
        if environment.is_none_or(|environment| environment["workspace"].is_null()) {
            body.insert("workspace".into(), json!(self.workspace.text()));
        }
        match environment {
            Some(environment) => {
                body.insert("environment".into(), environment["name"].clone());
            }
            None => {
                body.insert("allow_writes".into(), json!(self.writes.is_checked()));
                body.insert("allow_exec".into(), json!(self.exec.is_checked()));
                body.insert("allow_web".into(), json!(self.web.is_checked()));
                let mode = self
                    .approval
                    .selected()
                    .and_then(|index| APPROVAL_MODES.get(index))
                    .map_or("ask", |(name, _)| name);
                body.insert("approval_mode".into(), json!(mode));
            }
        }
        if let Some(name) = self
            .preset
            .selected()
            .filter(|index| *index > 0)
            .and_then(|index| self.options["presets"].get(index - 1))
            .and_then(|preset| preset["name"].as_str())
        {
            body.insert("preset".into(), json!(name));
        }
        Value::Object(body)
    }

    fn show_message(&self, message: Option<Message>) {
        match message {
            Some(message) => {
                self.error.set_text(&message.text);
                self.error.set_color(message.color);
            }
            None => self.error.set_text(""),
        }
    }
}
