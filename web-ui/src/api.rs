//! The server's JSON API, called with `fetch`.

use js_sys::futures::JsFuture;
use serde_json::Value;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{Headers, Request, RequestInit, Response};

pub async fn get(path: &str) -> Result<Value, String> {
    request("GET", path, None).await
}

pub async fn post(path: &str, body: &Value) -> Result<Value, String> {
    request("POST", path, Some(body)).await
}

pub async fn delete(path: &str) -> Result<Value, String> {
    request("DELETE", path, None).await
}

/// The JSON of the response, `null` when it has none. A failure carries the
/// server's `error`. Without the token (401) the page reloads, which shows
/// how to get in.
async fn request(method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
    let window = web_sys::window().ok_or("no window")?;
    let init = RequestInit::new();
    init.set_method(method);
    if let Some(body) = body {
        let headers = Headers::new().map_err(describe)?;
        headers
            .set("Content-Type", "application/json")
            .map_err(describe)?;
        init.set_headers(&headers);
        init.set_body(&JsValue::from_str(&body.to_string()));
    }
    let request = Request::new_with_str_and_init(path, &init).map_err(describe)?;
    let response: Response = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(describe)?
        .dyn_into()
        .map_err(describe)?;
    let text = JsFuture::from(response.text().map_err(describe)?)
        .await
        .map_err(describe)?
        .as_string()
        .unwrap_or_default();
    let value = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::Null)
    };
    if response.ok() {
        return Ok(value);
    }
    if response.status() == 401 {
        crate::dom::reload();
    }
    Err(value["error"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| format!("HTTP {}", response.status())))
}

fn describe(error: JsValue) -> String {
    error
        .as_string()
        .or_else(|| {
            error
                .dyn_ref::<js_sys::Error>()
                .map(|error| String::from(error.message()))
        })
        .unwrap_or_else(|| "request failed".to_string())
}
