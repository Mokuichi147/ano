//! What the page needs beyond naui's API, through the DOM elements that
//! naui's web backend exposes.

use naui::Widget;

/// Replace the contents of `widget` with `html`, which the server rendered
/// from Markdown with raw HTML escaped. naui keeps the element behind
/// `as_backend`, its escape hatch to the platform's widget.
pub fn set_html(widget: &dyn Widget, html: &str) {
    widget.as_backend().native_element().set_inner_html(html);
}

/// Whether the page is in a secure context (https or this computer), where
/// the browser offers the clipboard.
pub fn secure_context() -> bool {
    web_sys::window().is_some_and(|window| window.is_secure_context())
}

/// How far `widget`, which scrolls, is scrolled down.
pub fn scroll_top(widget: &dyn Widget) -> i32 {
    widget.as_backend().native_element().scroll_top()
}

pub fn set_scroll_top(widget: &dyn Widget, top: i32) {
    widget.as_backend().native_element().set_scroll_top(top);
}

pub fn reload() {
    if let Some(window) = web_sys::window() {
        let _ = window.location().reload();
    }
}
