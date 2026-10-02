//! What the page needs beyond naui's common API, through the DOM elements
//! that naui's web backend exposes.

use naui::{Scroll, TextArea, Widget};
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::{HtmlElement, HtmlTextAreaElement, KeyboardEvent};

/// Add CSS classes of the page's stylesheet to `widget`.
pub fn add_class(widget: &dyn Widget, classes: &str) {
    let list = widget.native_element().class_list();
    for class in classes.split_whitespace() {
        let _ = list.add_1(class);
    }
}

pub fn remove_class(widget: &dyn Widget, class: &str) {
    let _ = widget.native_element().class_list().remove_1(class);
}

/// Show or hide `widget` without removing it from its container.
pub fn set_visible(widget: &dyn Widget, visible: bool) {
    let _ = widget
        .native_element()
        .toggle_attribute_with_force("hidden", !visible);
}

/// Show `text` when the pointer rests on `widget`; empty removes it.
pub fn set_title(widget: &dyn Widget, text: &str) {
    let element = widget.native_element();
    let _ = if text.is_empty() {
        element.remove_attribute("title")
    } else {
        element.set_attribute("title", text)
    };
}

/// Replace the contents of `widget` with `html`, which the server rendered
/// from Markdown with raw HTML escaped.
pub fn set_html(widget: &dyn Widget, html: &str) {
    widget.native_element().set_inner_html(html);
}

fn scroll_element(scroll: &Scroll) -> Option<HtmlElement> {
    scroll.native_element().dyn_into().ok()
}

/// Whether the reader is at (or near) the end of `scroll`, so new content
/// should keep it there.
pub fn near_end(scroll: &Scroll) -> bool {
    scroll_element(scroll).is_none_or(|element| {
        element.scroll_height() - element.scroll_top() - element.client_height() < 120
    })
}

pub fn scroll_to_end(scroll: &Scroll) {
    if let Some(element) = scroll_element(scroll) {
        element.set_scroll_top(element.scroll_height());
    }
}

/// A listener on a DOM element, removed when this is dropped.
pub struct Listener {
    element: HtmlTextAreaElement,
    kind: &'static str,
    callback: Closure<dyn Fn(KeyboardEvent)>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self
            .element
            .remove_event_listener_with_callback(self.kind, self.callback.as_ref().unchecked_ref());
    }
}

/// Call `submit` when Enter is pressed in `area` without Shift, while the
/// returned listener lives. Enter that confirms an IME conversion does not
/// count.
pub fn on_enter(area: &TextArea, submit: impl Fn() + 'static) -> Option<Listener> {
    let element = area
        .native_element()
        .dyn_into::<HtmlTextAreaElement>()
        .ok()?;
    let callback = Closure::<dyn Fn(KeyboardEvent)>::new(move |event: KeyboardEvent| {
        if event.key() == "Enter"
            && !event.shift_key()
            && !event.is_composing()
            && event.key_code() != 229
        {
            event.prevent_default();
            submit();
        }
    });
    element
        .add_event_listener_with_callback("keydown", callback.as_ref().unchecked_ref())
        .ok()?;
    Some(Listener {
        element,
        kind: "keydown",
        callback,
    })
}

pub fn focus(widget: &dyn Widget) {
    if let Ok(element) = widget.native_element().dyn_into::<HtmlElement>() {
        let _ = element.focus();
    }
}

pub fn reload() {
    if let Some(window) = web_sys::window() {
        let _ = window.location().reload();
    }
}
