//! The home page: what ano knows and keeps for the user, from
//! `/api/home`. Each part shows only when its feature is enabled: the
//! character, ideas and articles of the feed (to rate, mark done, or carry
//! out), active objectives, upcoming routines, spaces, and what is
//! remembered.

use crate::{
    api,
    app::{App, Message},
};
use naui::{web::WebWidgetExt, Align, Label, Orientation, Padding, Stack, TextColor, TextStyle};
use serde_json::{json, Value};
use std::rc::Rc;

/// Build the home page from `home`, as `/api/home` returns it.
pub fn build(app: &Rc<App>, home: &Value) -> naui::Result<Stack> {
    let ui = &app.ui;
    let page = ui.stack(Orientation::Vertical)?;
    page.set_spacing(10.0);
    page.set_padding(Padding::all(16.0));
    page.set_align(Align::Fill);
    page.add_class("home");

    let greeting = match home["character"]["name"].as_str() {
        Some(name) => format!("{name} のホーム"),
        None => "ホーム".to_string(),
    };
    let heading = ui.label(&greeting)?;
    heading.set_style(TextStyle::Heading);
    heading.add_class("home-title");
    page.append(&heading);
    if let Some(tagline) = home["character"]["tagline"]
        .as_str()
        .filter(|t| !t.is_empty())
    {
        page.append(&secondary(app, tagline)?);
    }
    let new = ui.button("＋ 新しいセッション")?;
    new.on_click({
        let app = Rc::clone(app);
        move || app.show_setup(None)
    });
    page.append(&new);

    let mut shown = false;
    if let Some(items) = home["feed"].as_array() {
        shown = true;
        section(app, &page, "フィード")?;
        if items.is_empty() {
            page.append(&secondary(app, "新しい提案や記事はありません。")?);
        }
        for item in items {
            page.append(&feed_item(app, item)?);
        }
    }
    if let Some(objectives) = home["objectives"].as_array() {
        shown = true;
        section(app, &page, "目標")?;
        if objectives.is_empty() {
            page.append(&secondary(app, "進行中の目標はありません。")?);
        }
        for objective in objectives {
            page.append(&text(
                app,
                objective["summary"].as_str().unwrap_or_default(),
            )?);
        }
    }
    if let Some(routines) = home["routines"].as_array() {
        shown = true;
        section(app, &page, "これからの予定")?;
        if routines.is_empty() {
            page.append(&secondary(app, "リマインダーや定期作業はありません。")?);
        }
        for routine in routines {
            let kind = if routine["kind"] == "task" {
                "作業"
            } else {
                "リマインダー"
            };
            page.append(&text(
                app,
                &format!(
                    "{} — {}（{kind}）",
                    routine["at"].as_str().unwrap_or_default(),
                    routine["title"].as_str().unwrap_or_default()
                ),
            )?);
        }
    }
    if let Some(spaces) = home["spaces"].as_array() {
        shown = true;
        section(app, &page, "スペース")?;
        if spaces.is_empty() {
            page.append(&secondary(
                app,
                "会話で「〜のトラッカーを作って」と頼むと、ここに並びます。",
            )?);
        }
        for space in spaces {
            let name = space["name"].as_str().unwrap_or_default().to_string();
            let open = ui.button(space["title"].as_str().unwrap_or(&name))?;
            open.add_class("session-item");
            open.set_tooltip(Some("新しいタブで開きます"));
            open.on_click(move || open_tab(&format!("/spaces/{name}")));
            page.append(&open);
        }
    }
    if let Some(facts) = home["memory"].as_array() {
        shown = true;
        section(app, &page, "覚えていること")?;
        if facts.is_empty() {
            page.append(&secondary(app, "まだ何も覚えていません。")?);
        }
        for fact in facts.iter().rev().take(20) {
            page.append(&text(
                app,
                &format!("・{}", fact.as_str().unwrap_or_default()),
            )?);
        }
    }
    if !shown {
        page.append(&secondary(
            app,
            "設定でキャラクター・記憶・目標・ルーティン・フィード・スペースを有効にすると、ここにまとめて表示します。",
        )?);
    }
    Ok(page)
}

fn section(app: &App, page: &Stack, title: &str) -> naui::Result<()> {
    let label = app.ui.label(title)?;
    label.set_style(TextStyle::Heading);
    label.add_class("home-section");
    page.append(&label);
    Ok(())
}

fn text(app: &App, content: &str) -> naui::Result<Label> {
    let label = app.ui.label(content)?;
    label.set_selectable(true);
    label.set_wrap(true);
    Ok(label)
}

fn secondary(app: &App, content: &str) -> naui::Result<Label> {
    let label = text(app, content)?;
    label.set_color(TextColor::Secondary);
    Ok(label)
}

/// One item of the feed: its title and text, and what can be done with it.
fn feed_item(app: &Rc<App>, item: &Value) -> naui::Result<Stack> {
    let ui = &app.ui;
    let card = ui.stack(Orientation::Vertical)?;
    card.set_spacing(6.0);
    card.set_padding(Padding::all(12.0));
    card.set_align(Align::Fill);
    card.add_class("feed-item");
    let kind = match item["kind"].as_str() {
        Some("idea") => "提案",
        Some("article") => "記事",
        _ => "報告",
    };
    let rated = match item["rating"].as_i64() {
        Some(1) => " 👍",
        Some(-1) => " 👎",
        _ => "",
    };
    let title = ui.label(&format!(
        "{kind}：{}{rated}",
        item["title"].as_str().unwrap_or_default()
    ))?;
    title.set_style(TextStyle::Heading);
    title.add_class("feed-title");
    title.set_wrap(true);
    card.append(&title);
    if let Some(body) = item["body"].as_str().filter(|body| !body.is_empty()) {
        card.append(&secondary(app, body)?);
    }
    let actions = ui.stack(Orientation::Horizontal)?;
    actions.set_spacing(8.0);
    actions.add_class("feed-actions");
    let id = item["id"].as_str().unwrap_or_default().to_string();
    if let Some(action) = item["action"].as_str() {
        let run = ui.button("やってみる")?;
        run.set_tooltip(Some(action));
        let (app, id, action) = (Rc::clone(app), id.clone(), action.to_string());
        run.on_click(move || carry_out(&app, &id, &action));
        actions.append(&run);
    }
    if let Some(url) = item["url"].as_str() {
        let open = ui.button("開く")?;
        let url = url.to_string();
        open.on_click(move || open_tab(&url));
        actions.append(&open);
    }
    for (label, change) in [
        ("👍", json!({"rating": 1})),
        ("👎", json!({"rating": -1})),
        ("済み", json!({"done": true})),
    ] {
        let button = ui.button(label)?;
        let (app, id) = (Rc::clone(app), id.clone());
        button.on_click(move || update(&app, &id, change.clone()));
        actions.append(&button);
    }
    card.append(&actions);
    Ok(card)
}

/// Rate or mark the item `id`, then show the home page again.
fn update(app: &Rc<App>, id: &str, change: Value) {
    let (app, path) = (Rc::clone(app), format!("/api/feed/{id}"));
    app.clone().spawn(async move {
        if let Err(error) = api::post(&path, &change).await {
            web_sys::console::error_1(&error.into());
        }
        app.show_home().await;
    });
}

/// Start a session with the default settings, ask it to carry out
/// `action`, and mark the idea done.
fn carry_out(app: &Rc<App>, id: &str, action: &str) {
    let (app, id, action) = (Rc::clone(app), id.to_string(), action.to_string());
    app.clone().spawn(async move {
        let status = match api::post("/api/sessions", &json!({})).await {
            Ok(status) => status,
            Err(error) => {
                // Shown above the form, which can start a session another way.
                app.show_setup(Some(Message::error(format!(
                    "提案を始められませんでした: {error}"
                ))));
                return;
            }
        };
        let session = status["id"].as_str().unwrap_or_default();
        if let Err(error) = api::post(
            &format!("/api/sessions/{session}/messages"),
            &json!({"text": action}),
        )
        .await
        {
            web_sys::console::error_1(&error.into());
        }
        api::post(&format!("/api/feed/{id}"), &json!({"done": true}))
            .await
            .ok();
        app.open_session(&status);
    });
}

fn open_tab(url: &str) {
    if let Some(window) = web_sys::window() {
        window.open_with_url_and_target(url, "_blank").ok();
    }
}
