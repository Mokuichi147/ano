//! Reading optional arguments of tool calls.

use anyhow::{bail, Context, Result};
use serde_json::Value;

pub(super) fn optional_string<'a>(
    arguments: &'a Value,
    name: &str,
    default: &'a str,
) -> Result<&'a str> {
    match arguments
        .as_object()
        .context("arguments must be an object")?
        .get(name)
    {
        None => Ok(default),
        Some(value) => value
            .as_str()
            .with_context(|| format!("{name} must be a string")),
    }
}

pub(super) fn optional_integer(
    arguments: &Value,
    name: &str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64> {
    let value = match arguments
        .as_object()
        .context("arguments must be an object")?
        .get(name)
    {
        None => default,
        Some(value) => value
            .as_u64()
            .with_context(|| format!("{name} must be a non-negative integer"))?,
    };
    if !(min..=max).contains(&value) {
        bail!("{name} must be between {min} and {max}");
    }
    Ok(value)
}

pub(super) fn optional_bool(arguments: &Value, name: &str) -> Result<bool> {
    match arguments
        .as_object()
        .context("arguments must be an object")?
        .get(name)
    {
        None | Some(Value::Null) => Ok(false),
        Some(value) => value
            .as_bool()
            .with_context(|| format!("{name} must be a boolean")),
    }
}
