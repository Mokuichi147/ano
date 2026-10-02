//! A session's events, followed with `EventSource`. The browser reconnects
//! by itself and sends the last event's id, so the server resumes after it.

use serde_json::Value;
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::{EventSource, MessageEvent};

/// Follows the events while it lives.
pub struct EventStream {
    source: EventSource,
    _on_message: Closure<dyn Fn(MessageEvent)>,
    _on_error: Closure<dyn Fn()>,
}

impl EventStream {
    /// Call `on_event` with each event of session `id`, and `on_closed` when
    /// the browser gives up reconnecting (e.g. the session or the server is
    /// gone).
    pub fn open(
        id: &str,
        on_event: impl Fn(Value) + 'static,
        on_closed: impl Fn() + 'static,
    ) -> Option<Self> {
        let source = EventSource::new(&format!("/api/sessions/{id}/events")).ok()?;
        let on_message = Closure::<dyn Fn(MessageEvent)>::new(move |message: MessageEvent| {
            if let Some(Ok(event)) = message
                .data()
                .as_string()
                .map(|data| serde_json::from_str::<Value>(&data))
            {
                on_event(event);
            }
        });
        source.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        let watched = source.clone();
        let on_error = Closure::<dyn Fn()>::new(move || {
            if watched.ready_state() == EventSource::CLOSED {
                on_closed();
            }
        });
        source.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        Some(Self {
            source,
            _on_message: on_message,
            _on_error: on_error,
        })
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.source.set_onmessage(None);
        self.source.set_onerror(None);
        self.source.close();
    }
}
