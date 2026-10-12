//! The browser UI of `ano web`, built with naui's DOM backend.
//!
//! The page talks to the server's JSON API with `fetch` and follows a
//! session's events with `EventSource`. It shows one session: the form that
//! starts one when none is open, and otherwise the timeline of the session's
//! events with its plan, usage, and settings beside it.
//!
//! The CSS classes the page's stylesheet uses are set with naui's
//! `WebWidgetExt`. What naui has no word for (Markdown as HTML) goes through
//! the DOM elements naui exposes (`dom`).

#![cfg(target_arch = "wasm32")]

mod api;
mod app;
mod chat;
mod dom;
mod events;
mod setup;
mod timeline;

naui::entry!(naui::Settings::new("ano"), app::build);
