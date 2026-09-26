use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub enum InputPart {
    Text(String),
    Image(PathBuf),
    Audio(PathBuf),
    ImageData { data: String, mime_type: String },
    AudioData { data: String, format: String },
}

/// Builds the multimodal `input` array for the Responses API.
///
/// Text and images are sent as a user message. Audio is sent as a native
/// `input_audio` item with its original bytes base64-encoded; it is never
/// sent through the transcription endpoint.
pub async fn build_user_input(parts: &[InputPart]) -> Result<Value> {
    let mut message_content = Vec::new();
    let mut input_items = Vec::new();

    for part in parts {
        match part {
            InputPart::Text(text) if !text.trim().is_empty() => {
                message_content.push(json!({"type": "input_text", "text": text}));
            }
            InputPart::Image(path) => {
                message_content.push(json!({
                    "type": "input_image",
                    "image_url": image_data_url(path).await?,
                    "detail": "auto"
                }));
            }
            InputPart::ImageData { data, mime_type } => {
                message_content.push(json!({
                    "type": "input_image",
                    "image_url": image_data_url_from_base64(data, mime_type)?,
                    "detail": "auto"
                }));
            }
            InputPart::Audio(path) => {
                input_items.push(json!({
                    "type": "input_audio",
                    "input_audio": {
                        "data": audio_base64(path).await?,
                        "format": audio_format(path)?
                    }
                }));
            }
            InputPart::AudioData { data, format } => {
                input_items.push(json!({
                    "type": "input_audio",
                    "input_audio": {
                        "data": normalized_base64(data)?,
                        "format": audio_format_name(format)?
                    }
                }));
            }
            InputPart::Text(_) => {}
        }
    }

    if !message_content.is_empty() {
        input_items.insert(
            0,
            json!({
                "role": "user",
                "content": message_content,
            }),
        );
    }

    if input_items.is_empty() {
        bail!("at least one non-empty text, image, or audio input is required");
    }

    Ok(Value::Array(input_items))
}

async fn image_data_url(path: &Path) -> Result<String> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("failed to read image file {}", path.display()))?;
    let mime = mime_guess::from_path(path)
        .first_or_octet_stream()
        .essence_str()
        .to_string();
    if !mime.starts_with("image/") {
        bail!(
            "{} is not an image file (detected MIME type {mime})",
            path.display()
        );
    }

    Ok(format!("data:{mime};base64,{}", STANDARD.encode(bytes)))
}

fn image_data_url_from_base64(data: &str, mime_type: &str) -> Result<String> {
    if !mime_type.starts_with("image/") {
        bail!("image MIME type must start with image/; received {mime_type}");
    }
    Ok(format!(
        "data:{mime_type};base64,{}",
        normalized_base64(data)?
    ))
}

async fn audio_base64(path: &Path) -> Result<String> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("failed to read audio file {}", path.display()))?;
    Ok(STANDARD.encode(bytes))
}

fn audio_format(path: &Path) -> Result<&'static str> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase());
    match extension.as_deref() {
        Some("mp3") => Ok("mp3"),
        Some("wav") => Ok("wav"),
        Some(extension) => bail!(
            "Responses API input_audio currently accepts mp3 or wav, but {} has .{extension}",
            path.display()
        ),
        None => bail!(
            "audio file {} needs an .mp3 or .wav extension",
            path.display()
        ),
    }
}

fn audio_format_name(format: &str) -> Result<&'static str> {
    match format.to_ascii_lowercase().as_str() {
        "mp3" => Ok("mp3"),
        "wav" => Ok("wav"),
        _ => bail!("native input_audio format must be mp3 or wav"),
    }
}

fn normalized_base64(data: &str) -> Result<String> {
    let bytes = STANDARD
        .decode(data)
        .context("multimodal input data is not valid base64")?;
    Ok(STANDARD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::{build_user_input, InputPart};
    use std::path::PathBuf;

    #[tokio::test]
    async fn sends_audio_as_native_input_audio() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("voice.wav");
        tokio::fs::write(&path, [1_u8, 2_u8]).await.unwrap();

        let input = build_user_input(&[
            InputPart::Text("hello".into()),
            InputPart::Audio(PathBuf::from(&path)),
        ])
        .await
        .unwrap();

        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[1]["type"], "input_audio");
        assert_eq!(input[1]["input_audio"]["format"], "wav");
        assert_eq!(input[1]["input_audio"]["data"], "AQI=");
    }

    #[tokio::test]
    async fn accepts_inline_audio_without_transcription() {
        let input = build_user_input(&[InputPart::AudioData {
            data: "AQI=".into(),
            format: "WAV".into(),
        }])
        .await
        .unwrap();

        assert_eq!(input[0]["type"], "input_audio");
        assert_eq!(input[0]["input_audio"]["format"], "wav");
        assert_eq!(input[0]["input_audio"]["data"], "AQI=");
    }
}
