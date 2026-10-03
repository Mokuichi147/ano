//! The server's JSON API, called with `fetch`.

use js_sys::futures::JsFuture;
use serde_json::Value;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{Headers, Request, RequestInit, Response};

pub async fn get(path: &str) -> Result<Value, String> {
    request("GET", path, None).await.map_err(|(error, _)| error)
}

pub async fn post(path: &str, body: &Value) -> Result<Value, String> {
    request("POST", path, Some(body))
        .await
        .map_err(|(error, _)| error)
}

/// Like [`post`], with the body of a failed response, which may say more
/// than its `error`.
pub async fn post_detailed(path: &str, body: &Value) -> Result<Value, (String, Value)> {
    request("POST", path, Some(body)).await
}

pub async fn delete(path: &str) -> Result<Value, String> {
    request("DELETE", path, None)
        .await
        .map_err(|(error, _)| error)
}

/// The JSON of the response, `null` when it has none. A failure carries the
/// server's `error` and the body of the response. Without the token (401)
/// the page reloads, which shows how to get in.
async fn request(method: &str, path: &str, body: Option<&Value>) -> Result<Value, (String, Value)> {
    send(method, path, body)
        .await
        .map_err(|error| (error, Value::Null))?
}

/// The response, or why there is none.
async fn send(
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Result<Value, (String, Value)>, String> {
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
        return Ok(Ok(value));
    }
    if response.status() == 401 {
        crate::dom::reload();
    }
    let error = value["error"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| format!("HTTP {}", response.status()));
    Ok(Err((error, value)))
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
