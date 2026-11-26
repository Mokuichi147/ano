//! The settings page: every setting of the config file, built from the
//! schema `/api/settings` sends with the file's values.
//!
//! The page edits a copy of the values and sends all of it on save; the
//! server writes only what changed. A value equal to its default is left out
//! of the file. Saved settings take effect when the server restarts, which
//! the page offers to do.

use crate::{api, app::App};
use naui::{
    web::WebWidgetExt, Align, Button, DialogButtons, DialogResponse, Label, Orientation, Padding,
    Sizing, Stack, TextColor, TextStyle,
};
use serde_json::{json, Map, Value};
use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    rc::Rc,
};

/// A step into the values: a key of a table or an index of an array.
#[derive(Clone)]
enum Step {
    Key(String),
    Index(usize),
}

type Path = Vec<Step>;

fn key(path: &Path) -> String {
    path.iter()
        .map(|step| match step {
            Step::Key(key) => key.clone(),
            Step::Index(index) => index.to_string(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn with(path: &Path, step: Step) -> Path {
    let mut path = path.clone();
    path.push(step);
    path
}

/// The value at `path`, if any.
fn lookup<'a>(value: &'a Value, path: &Path) -> Option<&'a Value> {
    path.iter().try_fold(value, |value, step| match step {
        Step::Key(key) => value.get(key),
        Step::Index(index) => value.get(index),
    })
}

/// The table or array at `path`, made when missing.
fn reach<'a>(value: &'a mut Value, path: &[Step]) -> &'a mut Value {
    path.iter().fold(value, |value, step| match step {
        Step::Key(key) => {
            if !value.is_object() {
                *value = Value::Object(Map::new());
            }
            value
                .as_object_mut()
                .expect("an object")
                .entry(key.clone())
                .or_insert(Value::Null)
        }
        Step::Index(index) => {
            if !value.is_array() {
                *value = Value::Array(Vec::new());
            }
            let items = value.as_array_mut().expect("an array");
            while items.len() <= *index {
                items.push(Value::Object(Map::new()));
            }
            &mut items[*index]
        }
    })
}

pub struct Settings {
    app: Rc<App>,
    /// The page, which scrolls.
    page: Stack,
    schema: Value,
    defaults: Value,
    values: RefCell<Value>,
    revision: RefCell<String>,
    /// Where the sections are built.
    body: Stack,
    file: Label,
    status: Label,
    save: Button,
    restart: Button,
    /// The expanders open, by path, kept when the sections are built again.
    expanded: Rc<RefCell<HashSet<String>>>,
    /// Changes not saved yet.
    dirty: Cell<bool>,
    /// A save or a restart is under way.
    busy: Cell<bool>,
    /// The labels of number fields that hold something not a number.
    invalid: RefCell<HashSet<String>>,
}

/// Build the page from `settings`, as `/api/settings` returns it.
pub fn build(app: &Rc<App>, settings: &Value) -> naui::Result<Stack> {
    let ui = &app.ui;
    let page = ui.stack(Orientation::Vertical)?;
    page.set_spacing(10.0);
    page.set_padding(Padding::all(16.0));
    page.set_align(Align::Fill);
    page.add_class("settings");

    let heading = ui.label("設定")?;
    heading.set_style(TextStyle::Heading);
    heading.add_class("home-title");
    page.append(&heading);
    let file = ui.label("")?;
    file.set_style(TextStyle::Caption);
    file.set_color(TextColor::Secondary);
    file.set_wrap(true);
    file.set_selectable(true);
    page.append(&file);

    let bar = ui.stack(Orientation::Horizontal)?;
    bar.set_spacing(8.0);
    bar.set_align(Align::Center);
    bar.add_class("settings-bar");
    let save = ui.button("保存")?;
    let restart = ui.button("再起動して反映")?;
    restart.set_tooltip(Some(
        "実行中のターンを止め、保存した設定で ano web を起動し直します",
    ));
    let reload = ui.button("読み込み直す")?;
    reload.set_tooltip(Some(
        "保存していない変更を捨てて、設定ファイルを読み直します",
    ));
    let status = ui.label("")?;
    status.set_wrap(true);
    status.set_selectable(true);
    status.set_sizing(Sizing::fill_width());
    bar.append(&save);
    bar.append(&restart);
    bar.append(&reload);
    bar.append(&status);
    page.append(&bar);

    let body = ui.stack(Orientation::Vertical)?;
    body.set_spacing(8.0);
    body.set_align(Align::Fill);
    page.append(&body);

    let this = Rc::new(Settings {
        app: Rc::clone(app),
        page: page.clone(),
        schema: settings["schema"].clone(),
        defaults: settings["defaults"].clone(),
        values: RefCell::new(Value::Null),
        revision: RefCell::new(String::new()),
        body,
        file,
        status,
        save: save.clone(),
        restart: restart.clone(),
        expanded: Rc::new(RefCell::new(HashSet::new())),
        dirty: Cell::new(false),
        busy: Cell::new(false),
        invalid: RefCell::new(HashSet::new()),
    });
    this.load(settings);
    save.on_click({
        let this = Rc::clone(&this);
        move || this.save()
    });
    restart.on_click({
        let this = Rc::clone(&this);
        move || this.confirm_restart()
    });
    reload.on_click({
        let app = Rc::clone(app);
        move || {
            let app = Rc::clone(&app);
            app.clone().spawn(async move { app.show_settings().await });
        }
    });
    Ok(page)
}

impl Settings {
    /// Take the file's values and revision from `settings`, and build the
    /// sections for them.
    fn load(self: &Rc<Self>, settings: &Value) {
        let mut values = settings["values"].clone();
        if !values.is_object() {
            values = json!({});
        }
        *self.values.borrow_mut() = values;
        *self.revision.borrow_mut() = settings["revision"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let path = settings["path"].as_str().unwrap_or_default();
        self.file.set_text(&if settings["exists"] == true {
            path.to_string()
        } else {
            format!("{path}（未作成）")
        });
        self.dirty.set(false);
        self.invalid.borrow_mut().clear();
        self.render();
    }

    /// Build the sections again from the values, keeping open what was open
    /// and where the page was scrolled to.
    fn render(self: &Rc<Self>) {
        let top = crate::dom::scroll_top(&self.page);
        self.body.clear();
        if let Err(error) = self.build_groups() {
            web_sys::console::error_1(&error.to_string().into());
        }
        crate::dom::set_scroll_top(&self.page, top);
    }

    /// The tables under headings of what they are about: the features as a
    /// row of switches, then the others folded.
    fn build_groups(self: &Rc<Self>) -> naui::Result<()> {
        let fields = self.schema.as_array().cloned().unwrap_or_default();
        let find = |key: &str| fields.iter().find(|field| field["key"] == key);
        let mut placed: HashSet<&str> = HashSet::new();

        self.body.append(&self.heading("機能")?);
        let features: Vec<&Value> = FEATURES.iter().filter_map(|key| find(key)).collect();
        placed.extend(FEATURES);
        self.body.append(&self.features(&features)?);

        for (heading, keys) in GROUPS {
            self.body.append(&self.heading(heading)?);
            for key in *keys {
                if let Some(field) = find(key) {
                    placed.insert(key);
                    let path = vec![Step::Key((*key).into())];
                    self.body.append(&self.section(field, &path)?);
                }
            }
        }
        // Tables the page does not know a heading for yet.
        for field in &fields {
            let key = field["key"].as_str().unwrap_or_default();
            if !placed.contains(key) {
                let path = vec![Step::Key(key.into())];
                self.body.append(&self.section(field, &path)?);
            }
        }
        Ok(())
    }

    /// The features: a switch each, and their other settings folded below.
    fn features(self: &Rc<Self>, features: &[&Value]) -> naui::Result<Stack> {
        let ui = &self.app.ui;
        let card = ui.stack(Orientation::Vertical)?;
        card.set_spacing(8.0);
        card.set_padding(Padding::all(10.0));
        card.set_align(Align::Fill);
        card.add_class("settings-section");
        let switches = ui.flow()?;
        switches.set_spacing(20.0, 6.0);
        let details = ui.stack(Orientation::Vertical)?;
        details.set_spacing(10.0);
        details.set_padding(Padding::all(8.0));
        details.set_align(Align::Fill);
        for feature in features {
            let key = feature["key"].as_str().unwrap_or_default();
            let path = vec![Step::Key(key.into())];
            let fields = feature["fields"].as_array().cloned().unwrap_or_default();
            for field in &fields {
                let name = field["key"].as_str().unwrap_or_default();
                let inner = with(&path, Step::Key(name.into()));
                if name == "enabled" {
                    let toggle = self.checkbox(
                        feature["label"].as_str().unwrap_or_default(),
                        &inner,
                        &inner,
                        // Others follow it, so they are shown again.
                        NEEDS.iter().any(|(_, needs)| needs.contains(&key)),
                    )?;
                    toggle.set_tooltip(field["help"].as_str());
                    // Off while what it needs is off, whatever it says.
                    if let Some((_, needs)) = NEEDS.iter().find(|(name, _)| *name == key) {
                        toggle.set_enabled(needs.iter().all(|need| self.enabled(need)));
                    }
                    switches.append(&toggle);
                }
            }
            let rest: Vec<Value> = fields
                .into_iter()
                .filter(|field| field["key"] != "enabled")
                .collect();
            if let [only] = rest.as_slice() {
                // One key, such as the folder: a row named after the feature.
                let mut row = only.clone();
                row["label"] = feature["label"].clone();
                let name = only["key"].as_str().unwrap_or_default();
                let inner = with(&path, Step::Key(name.into()));
                details.append(&self.field(&row, &inner, &inner)?);
            } else if !rest.is_empty() {
                let group = json!({
                    "key": key,
                    "label": feature["label"],
                    "kind": "table",
                    "fields": rest,
                });
                details.append(&self.field(&group, &path, &path)?);
            }
        }
        card.append(&switches);
        let expander = ui.expander("保存先")?;
        expander.set_sizing(Sizing::fill_width());
        expander.set_child(&details);
        self.remember(&expander, &vec![Step::Key("features".into())]);
        card.append(&expander);
        Ok(card)
    }

    /// A checkbox for the switch at `path`, which leaves the key out of the
    /// file while it is at its default.
    fn checkbox(
        self: &Rc<Self>,
        label: &str,
        path: &Path,
        defaults: &Path,
        render: bool,
    ) -> naui::Result<naui::Checkbox> {
        let checkbox = self.app.ui.checkbox(label)?;
        let implied = self
            .default(defaults)
            .as_ref()
            .and_then(Value::as_bool)
            .unwrap_or(false);
        checkbox.set_checked(
            self.value(path)
                .as_ref()
                .and_then(Value::as_bool)
                .unwrap_or(implied),
        );
        let this = Rc::clone(self);
        let path = path.clone();
        checkbox.on_toggle(move |checked| {
            this.set(&path, (checked != implied).then(|| json!(checked)));
            if render {
                this.render();
            }
        });
        Ok(checkbox)
    }

    /// Whether the feature `key` is on, as the page sets it now.
    fn enabled(&self, key: &str) -> bool {
        let path = vec![Step::Key(key.into()), Step::Key("enabled".into())];
        self.value(&path)
            .or_else(|| self.default(&path))
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    }

    fn set_status(&self, text: &str, color: TextColor) {
        self.status.set_text(text);
        self.status.set_color(color);
    }

    fn changed(&self) {
        self.dirty.set(true);
        self.set_status("保存していない変更があります", TextColor::Warning);
    }

    fn value(&self, path: &Path) -> Option<Value> {
        lookup(&self.values.borrow(), path)
            .filter(|value| !value.is_null())
            .cloned()
    }

    /// Set the value at `path`, or leave it out of the file with `None`.
    fn set(&self, path: &Path, value: Option<Value>) {
        let mut values = self.values.borrow_mut();
        let (last, parent) = path.split_last().expect("a path");
        match value {
            Some(value) => *reach(&mut values, &with(&parent.to_vec(), last.clone())) = value,
            None => {
                if let Some(Value::Object(table)) = lookup_mut(&mut values, parent) {
                    if let Step::Key(key) = last {
                        table.remove(key);
                    }
                }
            }
        }
        drop(values);
        self.changed();
    }

    /// The default of the key at `defaults`, a path with `*` for entries.
    /// A provider takes what it leaves out from `[api]`, as the page sets it.
    fn default(&self, defaults: &Path) -> Option<Value> {
        if let [Step::Key(table), Step::Key(entry), Step::Key(name)] = defaults.as_slice() {
            if table == "providers" && entry == "*" && INHERITED.contains(&name.as_str()) {
                let api = vec![Step::Key("api".into()), Step::Key(name.clone())];
                if let Some(value) = self.value(&api) {
                    return Some(value);
                }
            }
        }
        lookup(&self.defaults, defaults)
            .filter(|value| !value.is_null())
            .cloned()
    }

    /// A top-level table of the config file, folded.
    fn section(self: &Rc<Self>, field: &Value, path: &Path) -> naui::Result<Stack> {
        let ui = &self.app.ui;
        let wrapper = ui.stack(Orientation::Vertical)?;
        wrapper.set_align(Align::Fill);
        wrapper.add_class("settings-section");
        let expander = ui.expander(field["label"].as_str().unwrap_or_default())?;
        expander.set_sizing(Sizing::fill_width());
        expander.set_tooltip(field["help"].as_str());
        let inner = ui.stack(Orientation::Vertical)?;
        inner.set_spacing(6.0);
        inner.set_padding(Padding::all(8.0));
        inner.set_align(Align::Fill);
        self.fill(&inner, field, path, path)?;
        expander.set_child(&inner);
        self.remember(&expander, path);
        wrapper.append(&expander);
        Ok(wrapper)
    }

    /// Keep `expander` open across builds while the user keeps it open.
    fn remember(&self, expander: &naui::Expander, path: &Path) {
        let name = key(path);
        expander.set_expanded(self.expanded.borrow().contains(&name));
        let expanded = Rc::clone(&self.expanded);
        expander.on_toggle(move |open| {
            let mut expanded = expanded.borrow_mut();
            if open {
                expanded.insert(name.clone());
            } else {
                expanded.remove(&name);
            }
        });
    }

    /// The inside of a table, a named table, or an array of tables at `path`.
    fn fill(
        self: &Rc<Self>,
        container: &Stack,
        field: &Value,
        path: &Path,
        defaults: &Path,
    ) -> naui::Result<()> {
        match field["kind"].as_str() {
            Some("table") => {
                for inner in field["fields"].as_array().into_iter().flatten() {
                    let name = inner["key"].as_str().unwrap_or_default().to_string();
                    container.append(&self.field(
                        inner,
                        &with(path, Step::Key(name.clone())),
                        &with(defaults, Step::Key(name)),
                    )?);
                }
            }
            Some("named") => self.named(container, field, path, defaults)?,
            Some("list") => self.list(container, field, path, defaults)?,
            _ => {}
        }
        Ok(())
    }

    /// One key: its name on the left and its input on the right; what it
    /// means is in the tooltip.
    fn field(self: &Rc<Self>, field: &Value, path: &Path, defaults: &Path) -> naui::Result<Stack> {
        let ui = &self.app.ui;
        let label = field["label"].as_str().unwrap_or_default();
        let help = field["help"].as_str();
        let value = self.value(path);
        let default = self.default(defaults);
        match field["kind"].as_str().unwrap_or_default() {
            "table" | "named" | "list" => {
                let group = ui.stack(Orientation::Vertical)?;
                group.set_spacing(6.0);
                group.set_padding(Padding::all(10.0));
                group.set_align(Align::Fill);
                group.add_class("settings-group");
                let caption = self.caption(label)?;
                caption.set_tooltip(help);
                group.append(&caption);
                self.fill(&group, field, path, defaults)?;
                Ok(group)
            }
            kind => {
                let row = ui.stack(Orientation::Horizontal)?;
                row.set_spacing(12.0);
                row.set_align(Align::Fill);
                row.add_class("settings-row");
                row.set_tooltip(help);
                let name = ui.label(if kind == "bool" { "" } else { label })?;
                name.set_color(TextColor::Secondary);
                name.add_class("settings-name");
                row.append(&name);
                if kind == "bool" {
                    row.append(&self.checkbox(label, path, defaults, false)?);
                } else {
                    self.input(&row, kind, field, path, value, default)?;
                }
                Ok(row)
            }
        }
    }

    /// The input of a key that holds a value.
    fn input(
        self: &Rc<Self>,
        row: &Stack,
        kind: &str,
        field: &Value,
        path: &Path,
        value: Option<Value>,
        default: Option<Value>,
    ) -> naui::Result<()> {
        let ui = &self.app.ui;
        let this = Rc::clone(self);
        let path = path.clone();
        match kind {
            "choice" => {
                let options: Vec<String> = field["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                let first = match default.as_ref().and_then(Value::as_str) {
                    Some(default) => format!("既定（{default}）"),
                    None => "指定なし".to_string(),
                };
                let mut items = vec![first];
                items.extend(options.iter().cloned());
                let combo = ui.combo_box()?;
                combo.set_items(&items);
                let selected = value
                    .as_ref()
                    .and_then(Value::as_str)
                    .and_then(|value| options.iter().position(|option| option == value))
                    .map_or(0, |index| index + 1);
                combo.set_selected(selected);
                combo.set_sizing(Sizing::fill_width().max_width(360.0));
                combo.set_accessible_label(field["label"].as_str());
                combo.on_select(move |index| {
                    let value = index
                        .checked_sub(1)
                        .and_then(|index| options.get(index))
                        .map(|option| json!(option));
                    this.set(&path, value);
                });
                row.append(&combo);
            }
            "long_text" | "strings" | "pairs" => {
                let text = match (kind, &value) {
                    (_, None) => String::new(),
                    ("strings", Some(value)) => lines(value),
                    ("pairs", Some(Value::Object(pairs))) => pairs
                        .iter()
                        .map(|(name, value)| {
                            format!("{name}={}", value.as_str().unwrap_or_default())
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                    (_, Some(value)) => value.as_str().unwrap_or_default().to_string(),
                };
                let area = ui.text_area(&text)?;
                area.set_sizing(Sizing::fill_width());
                area.set_accessible_label(field["label"].as_str());
                area.add_class("settings-area");
                let placeholder = match (kind, &default) {
                    ("strings", Some(default)) => lines(default),
                    ("strings", None) => "1行に1つ".to_string(),
                    ("pairs", _) => "名前=値（1行に1つ）".to_string(),
                    _ => String::new(),
                };
                area.set_placeholder(&placeholder);
                let kind = kind.to_string();
                area.on_change(move |text| {
                    let value = match kind.as_str() {
                        "strings" => {
                            let items: Vec<&str> = text
                                .lines()
                                .map(str::trim)
                                .filter(|line| !line.is_empty())
                                .collect();
                            (!items.is_empty()).then(|| json!(items))
                        }
                        "pairs" => {
                            let pairs: Map<String, Value> = text
                                .lines()
                                .filter_map(|line| line.split_once('='))
                                .map(|(name, value)| (name.trim().to_string(), json!(value.trim())))
                                .filter(|(name, _)| !name.is_empty())
                                .collect();
                            (!pairs.is_empty()).then_some(Value::Object(pairs))
                        }
                        _ => (!text.trim().is_empty()).then(|| json!(text)),
                    };
                    this.set(&path, value);
                });
                row.append(&area);
            }
            _ => {
                let text = match &value {
                    Some(Value::String(text)) => text.clone(),
                    Some(Value::Number(number)) => number.to_string(),
                    _ => String::new(),
                };
                let input = ui.text_input(&text)?;
                input.set_sizing(Sizing::fill_width().max_width(560.0));
                input.set_accessible_label(field["label"].as_str());
                if let Some(default) = &default {
                    input.set_placeholder(&match default {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    });
                }
                let integer = kind == "integer";
                let label = field["label"].as_str().unwrap_or_default().to_string();
                input.on_change(move |text| {
                    let text = text.trim();
                    let number = text.parse::<i64>();
                    if integer && !text.is_empty() && number.is_err() {
                        this.invalid.borrow_mut().insert(label.clone());
                    } else {
                        this.invalid.borrow_mut().remove(&label);
                    }
                    if text.is_empty() {
                        this.set(&path, None);
                    } else if !integer {
                        this.set(&path, Some(json!(text)));
                    } else {
                        match text.parse::<i64>() {
                            Ok(number) => this.set(&path, Some(json!(number))),
                            Err(_) => this.set_status(
                                &format!("「{label}」には整数を入力してください"),
                                TextColor::Danger,
                            ),
                        }
                    }
                });
                row.append(&input);
            }
        }
        Ok(())
    }

    /// Tables by name: one folded part per entry, and a row to add one.
    fn named(
        self: &Rc<Self>,
        container: &Stack,
        field: &Value,
        path: &Path,
        defaults: &Path,
    ) -> naui::Result<()> {
        let ui = &self.app.ui;
        let names: Vec<String> = self
            .value(path)
            .and_then(|value| {
                value
                    .as_object()
                    .map(|table| table.keys().cloned().collect())
            })
            .unwrap_or_default();
        let entry_defaults = with(defaults, Step::Key("*".into()));
        for name in names {
            let entry = with(path, Step::Key(name.clone()));
            container.append(&self.entry(&name, field, &entry, &entry_defaults, {
                let this = Rc::clone(self);
                let entry = entry.clone();
                move || {
                    this.set(&entry, None);
                    this.render();
                }
            })?);
        }
        let add = ui.stack(Orientation::Horizontal)?;
        add.set_spacing(8.0);
        add.set_align(Align::Center);
        let name = ui.text_input("")?;
        name.set_placeholder("新しい名前");
        name.set_accessible_label(Some("追加する名前"));
        name.set_sizing(Sizing::fill_width().max_width(280.0));
        let button = ui.button("追加")?;
        add.append(&name);
        add.append(&button);
        button.on_click({
            let this = Rc::clone(self);
            let path = path.clone();
            let name = name.clone();
            move || {
                let new = name.text().trim().to_string();
                if new.is_empty() {
                    this.set_status("追加する名前を入力してください", TextColor::Danger);
                    return;
                }
                let entry = with(&path, Step::Key(new.clone()));
                if this.value(&entry).is_some() {
                    this.set_status(&format!("「{new}」はもうあります"), TextColor::Danger);
                    return;
                }
                this.set(&entry, Some(json!({})));
                this.expanded.borrow_mut().insert(key(&entry));
                this.render();
            }
        });
        container.append(&add);
        Ok(())
    }

    /// An array of tables: one folded part per item, and a button to add one.
    fn list(
        self: &Rc<Self>,
        container: &Stack,
        field: &Value,
        path: &Path,
        defaults: &Path,
    ) -> naui::Result<()> {
        let ui = &self.app.ui;
        let count = self
            .value(path)
            .and_then(|value| value.as_array().map(Vec::len))
            .unwrap_or(0);
        let title = field["title"].as_str().unwrap_or_default();
        let item_defaults = with(defaults, Step::Key("*".into()));
        for index in 0..count {
            let item = with(path, Step::Index(index));
            let name = self
                .value(&with(&item, Step::Key(title.into())))
                .and_then(|value| value.as_str().map(str::to_string))
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| format!("{}（名前なし）", index + 1));
            container.append(&self.entry(&name, field, &item, &item_defaults, {
                let this = Rc::clone(self);
                let path = path.clone();
                move || {
                    if let Some(Value::Array(items)) =
                        lookup_mut(&mut this.values.borrow_mut(), &path)
                    {
                        if index < items.len() {
                            items.remove(index);
                        }
                    }
                    this.changed();
                    this.render();
                }
            })?);
        }
        let row = ui.stack(Orientation::Horizontal)?;
        let add = ui.button("追加")?;
        row.append(&add);
        add.on_click({
            let this = Rc::clone(self);
            let path = path.clone();
            move || {
                let item = with(&path, Step::Index(count));
                this.set(&item, Some(json!({})));
                this.expanded.borrow_mut().insert(key(&item));
                this.render();
            }
        });
        container.append(&row);
        Ok(())
    }

    /// One entry of a named table or item of an array of tables, folded,
    /// with a button that removes it.
    fn entry(
        self: &Rc<Self>,
        name: &str,
        field: &Value,
        path: &Path,
        defaults: &Path,
        remove: impl FnMut() + 'static,
    ) -> naui::Result<Stack> {
        let ui = &self.app.ui;
        let wrapper = ui.stack(Orientation::Vertical)?;
        wrapper.set_align(Align::Fill);
        wrapper.add_class("settings-entry");
        let expander = ui.expander(name)?;
        expander.set_sizing(Sizing::fill_width());
        let inner = ui.stack(Orientation::Vertical)?;
        inner.set_spacing(10.0);
        inner.set_padding(Padding::all(8.0));
        inner.set_align(Align::Fill);
        // The entry's keys, as if it were a table of its own.
        let table = json!({"kind": "table", "fields": field["fields"]});
        self.fill(&inner, &table, path, defaults)?;
        let actions = ui.stack(Orientation::Horizontal)?;
        let delete = ui.button(&format!("「{name}」を削除"))?;
        delete.set_tooltip(Some("保存すると設定ファイルから消えます"));
        delete.on_click(remove);
        actions.append(&delete);
        inner.append(&actions);
        expander.set_child(&inner);
        self.remember(&expander, path);
        wrapper.append(&expander);
        Ok(wrapper)
    }

    /// The heading of a group of tables.
    fn heading(&self, text: &str) -> naui::Result<Label> {
        let label = self.app.ui.label(text)?;
        label.set_style(TextStyle::Heading);
        label.add_class("settings-heading");
        Ok(label)
    }

    fn caption(&self, text: &str) -> naui::Result<Label> {
        let label = self.app.ui.label(text)?;
        label.set_style(TextStyle::Heading);
        label.set_color(TextColor::Secondary);
        Ok(label)
    }

    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        self.save.set_enabled(!busy);
        self.restart.set_enabled(!busy);
    }

    fn save(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        if let Some(label) = self.invalid.borrow().iter().next() {
            self.set_status(
                &format!("「{label}」には整数を入力してください"),
                TextColor::Danger,
            );
            return;
        }
        self.set_busy(true);
        self.set_status("保存しています…", TextColor::Secondary);
        let body = json!({
            "revision": *self.revision.borrow(),
            "values": *self.values.borrow(),
        });
        let this = Rc::clone(self);
        self.app.spawn(async move {
            match api::put_detailed("/api/settings", &body).await {
                Ok(settings) => {
                    this.load(&settings);
                    this.set_status(
                        "保存しました。「再起動して反映」で使い始めます",
                        TextColor::Success,
                    );
                }
                Err((error, _)) => {
                    this.set_status(&format!("保存できませんでした: {error}"), TextColor::Danger)
                }
            }
            this.set_busy(false);
        });
    }

    fn confirm_restart(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        let Ok(dialog) = self.app.ui.dialog("ano web を再起動しますか？") else {
            return;
        };
        dialog.set_message(if self.dirty.get() {
            "保存していない変更は反映されず、捨てられます。実行中のターンは止まります。"
        } else {
            "保存した設定で起動し直します。実行中のターンは止まります（保存しているセッションは再起動後も開けます）。"
        });
        dialog.set_buttons(DialogButtons::new().primary("再起動").cancel("キャンセル"));
        let this = Rc::clone(self);
        dialog.on_response(move |response| {
            if response == DialogResponse::Primary {
                this.restart();
            }
        });
        dialog.open();
    }

    /// Ask the server to restart, wait for the new one, and load the page
    /// from it.
    fn restart(self: &Rc<Self>) {
        self.set_busy(true);
        self.set_status("再起動しています…", TextColor::Secondary);
        let this = Rc::clone(self);
        self.app.spawn(async move {
            let before = api::get("/api/options")
                .await
                .ok()
                .and_then(|options| options["instance"].as_str().map(str::to_string));
            if let Err(error) = api::post("/api/restart", &json!({})).await {
                this.set_status(
                    &format!("再起動できませんでした: {error}"),
                    TextColor::Danger,
                );
                this.set_busy(false);
                return;
            }
            for _ in 0..60 {
                sleep(1000).await;
                if let Ok(options) = api::get("/api/options").await {
                    let now = options["instance"].as_str().map(str::to_string);
                    if now.is_some() && now != before {
                        crate::dom::reload();
                        return;
                    }
                }
            }
            this.set_status(
                "再起動を確認できませんでした。ano web を起動したターミナルを確認してください",
                TextColor::Danger,
            );
            this.set_busy(false);
        });
    }
}

/// The features, shown as switches, in this order.
const FEATURES: &[&str] = &[
    "memory",
    "objectives",
    "routines",
    "agenda",
    "feed",
    "spaces",
    "skills",
    "character",
    "history",
    "events",
];

/// Keys a provider takes from `[api]` when it leaves them out.
const INHERITED: &[&str] = &["stream", "timeout_secs", "max_retries"];

/// Features that work only while others are on.
const NEEDS: &[(&str, &[&str])] = &[("events", &["history"]), ("agenda", &["feed", "routines"])];

/// The other tables, under the heading of what they are about.
const GROUPS: &[(&str, &[&str])] = &[
    ("モデル", &["agent", "api", "providers", "presets"]),
    ("実行", &["environments", "mcp_servers", "users"]),
    ("ano serve", &["webhook", "automation"]),
];

/// `value`, a list of strings, one per line.
fn lines(value: &Value) -> String {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

fn lookup_mut<'a>(value: &'a mut Value, path: &[Step]) -> Option<&'a mut Value> {
    path.iter().try_fold(value, |value, step| match step {
        Step::Key(key) => value.get_mut(key),
        Step::Index(index) => value.get_mut(index),
    })
}

/// Wait `ms` milliseconds.
async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = js_sys::futures::JsFuture::from(promise).await;
}
