//! HMAC-SHA256 request signatures with a timestamp window and replay cache.

use super::{unix_now, WebhookState};
use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub(super) type HmacSha256 = Hmac<Sha256>;

const SIGNATURE_HEADER: &str = "x-ano-signature";
const TIMESTAMP_HEADER: &str = "x-ano-timestamp";

/// Verify `X-Ano-Signature: sha256=<hex>` over `"<timestamp>.<payload>"`,
/// where `<timestamp>` is the `X-Ano-Timestamp` header in Unix seconds.
///
/// Returns the verified signature bytes, or `None` when authentication is
/// disabled.
pub(super) fn verify_signature(
    state: &WebhookState,
    headers: &HeaderMap,
    payload: &[u8],
    now: u64,
) -> std::result::Result<Option<Vec<u8>>, &'static str> {
    let Some(secret) = state.secret.as_deref() else {
        return if state.config.webhook.allow_unauthenticated {
            Ok(None)
        } else {
            Err("webhook authentication is not configured")
        };
    };
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());

    let timestamp = header(TIMESTAMP_HEADER).ok_or("missing X-Ano-Timestamp")?;
    let issued_at = timestamp
        .parse::<u64>()
        .map_err(|_| "invalid X-Ano-Timestamp")?;
    if now.abs_diff(issued_at) > state.config.webhook.signature_tolerance_secs {
        return Err("expired webhook signature");
    }

    let signature = header(SIGNATURE_HEADER).ok_or("missing X-Ano-Signature")?;
    let signature = signature.strip_prefix("sha256=").unwrap_or(signature);
    let provided = hex::decode(signature).map_err(|_| "invalid webhook signature")?;
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| "invalid webhook secret")?;
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(payload);
    mac.verify_slice(&provided)
        .map_err(|_| "invalid webhook signature")?;
    Ok(Some(provided))
}

/// Record a signature for the tolerance window. Returns `false` if it was
/// already used.
pub(super) fn remember_signature(state: &WebhookState, signature: Vec<u8>) -> bool {
    let now = unix_now();
    // Expire after the widest window in which the timestamp is still valid.
    let expires_at = now.saturating_add(
        state
            .config
            .webhook
            .signature_tolerance_secs
            .saturating_mul(2),
    );
    let mut seen = state
        .seen_signatures
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    seen.retain(|_, expiry| *expiry > now);
    seen.insert(signature, expires_at).is_none()
}
