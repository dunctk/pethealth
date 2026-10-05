use crate::{config::Config, domain::PetPhoto};
use anyhow::{Context, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rand::{Rng, distr::Alphanumeric};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use tokio::fs;

pub const MAX_PHOTO_BYTES: usize = 15 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct EyeAlignment {
    pub left_x: f64,
    pub left_y: f64,
    pub right_x: f64,
    pub right_y: f64,
    pub confidence: f64,
}

#[derive(Debug, Deserialize)]
struct VisionPoint {
    x: f64,
    y: f64,
}

#[derive(Debug, Deserialize)]
struct VisionEyes {
    found: bool,
    left_eye: Option<VisionPoint>,
    right_eye: Option<VisionPoint>,
    confidence: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletion {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    content: String,
}

pub async fn store_upload(
    config: &Config,
    household_id: i64,
    pet_id: i64,
    mime_type: &str,
    bytes: &[u8],
) -> anyhow::Result<String> {
    validate_image(mime_type, bytes)?;
    let extension = extension_for(mime_type)?;
    let storage_name: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect::<String>()
        .to_lowercase();
    let storage_name = format!("{storage_name}.{extension}");
    let directory = pet_directory(config, household_id, pet_id);
    fs::create_dir_all(&directory)
        .await
        .with_context(|| format!("failed to create photo directory {}", directory.display()))?;
    let path = directory.join(&storage_name);
    fs::write(&path, bytes)
        .await
        .with_context(|| format!("failed to store photo {}", path.display()))?;
    Ok(storage_name)
}

pub async fn remove_upload(
    config: &Config,
    household_id: i64,
    pet_id: i64,
    storage_name: &str,
) {
    let _ = fs::remove_file(photo_path(config, household_id, pet_id, storage_name)).await;
}

pub async fn load_photo(config: &Config, photo: &PetPhoto) -> anyhow::Result<Vec<u8>> {
    let path = photo_path(
        config,
        photo.household_id,
        photo.pet_id,
        &photo.storage_name,
    );
    fs::read(&path)
        .await
        .with_context(|| format!("failed to read photo {}", path.display()))
}

pub async fn detect_eyes(
    config: &Config,
    mime_type: &str,
    bytes: &[u8],
) -> anyhow::Result<EyeAlignment> {
    let api_key = config
        .llm_api_key
        .as_deref()
        .ok_or_else(|| anyhow!("AI is not configured"))?;
    validate_image(mime_type, bytes)?;
    let prompt = r#"Find the two visible eyes of the pet in this photo. Return JSON only:
{"found":true,"left_eye":{"x":0.25,"y":0.4},"right_eye":{"x":0.65,"y":0.4},"confidence":0.9}
Coordinates are normalized from 0 to 1 across the displayed image. "left_eye" means the eye on the LEFT SIDE OF THE IMAGE, not the animal's anatomical left. If both eyes cannot be located confidently, return {"found":false,"confidence":0.0}. Do not include markdown or commentary."#;
    let data_url = data_url(mime_type, bytes);
    let response = vision_request(
        config,
        api_key,
        vec![
            json!({"type":"text","text":prompt}),
            json!({"type":"image_url","image_url":{"url":data_url}}),
        ],
        220,
    )
    .await?;
    let parsed: VisionEyes =
        serde_json::from_str(strip_json_fence(&response)).context("invalid eye-coordinate JSON")?;
    if !parsed.found {
        bail!("both eyes were not confidently visible");
    }
    let left = parsed
        .left_eye
        .ok_or_else(|| anyhow!("left eye coordinate missing"))?;
    let right = parsed
        .right_eye
        .ok_or_else(|| anyhow!("right eye coordinate missing"))?;
    for value in [left.x, left.y, right.x, right.y] {
        if !(0.0..=1.0).contains(&value) {
            bail!("eye coordinate out of range");
        }
    }
    let distance = ((right.x - left.x).powi(2) + (right.y - left.y).powi(2)).sqrt();
    if distance < 0.05 {
        bail!("eye coordinates are too close together");
    }
    Ok(EyeAlignment {
        left_x: left.x,
        left_y: left.y,
        right_x: right.x,
        right_y: right.y,
        confidence: parsed.confidence.unwrap_or(0.0).clamp(0.0, 1.0),
    })
}

pub async fn compare_photos(
    config: &Config,
    pet_name: &str,
    from_photo: &PetPhoto,
    from_bytes: &[u8],
    to_photo: &PetPhoto,
    to_bytes: &[u8],
) -> anyhow::Result<String> {
    let api_key = config
        .llm_api_key
        .as_deref()
        .ok_or_else(|| anyhow!("AI is not configured"))?;
    validate_image(&from_photo.mime_type, from_bytes)?;
    validate_image(&to_photo.mime_type, to_bytes)?;
    let prompt = format!(
        "Compare these two photos of {pet_name}, first from {} and second from {}. Describe only visible changes in appearance. Pay particular attention to facial fullness/puffiness around the cheeks, muzzle, eyelids and area around the eyes, but also mention other clear appearance changes. Separate likely real visual changes from differences that could be caused by head angle, distance, expression, fur position or lighting. Be concise and cautious. Do not diagnose a condition, infer a medication side effect, or say a change is medically significant. End with one short sentence saying what would be worth showing a vet if the visual change persists. Plain text only.",
        from_photo.captured_at.format("%d %b %Y %H:%M"),
        to_photo.captured_at.format("%d %b %Y %H:%M"),
    );
    vision_request(
        config,
        api_key,
        vec![
            json!({"type":"text","text":prompt}),
            json!({"type":"image_url","image_url":{"url":data_url(&from_photo.mime_type, from_bytes)}}),
            json!({"type":"image_url","image_url":{"url":data_url(&to_photo.mime_type, to_bytes)}}),
        ],
        650,
    )
    .await
}

async fn vision_request(
    config: &Config,
    api_key: &str,
    content: Vec<serde_json::Value>,
    max_tokens: u32,
) -> anyhow::Result<String> {
    let endpoint = format!(
        "{}/chat/completions",
        config.llm_base_url.trim_end_matches('/')
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(config.llm_timeout_seconds))
        .build()?;
    let response = client
        .post(endpoint)
        .bearer_auth(api_key)
        .json(&json!({
            "model": config.llm_model,
            "messages": [{"role":"user","content":content}],
            "max_tokens": max_tokens,
            "temperature": 0,
            "provider": {"data_collection":"deny"}
        }))
        .send()
        .await
        .context("vision request failed")?;
    let status = response.status();
    let body = response.text().await.context("failed to read vision response")?;
    if !status.is_success() {
        bail!("vision model returned {status}: {}", truncate(&body, 240));
    }
    let parsed: ChatCompletion =
        serde_json::from_str(&body).context("invalid vision completion response")?;
    parsed
        .choices
        .into_iter()
        .next()
        .map(|choice| choice.message.content.trim().to_owned())
        .filter(|content| !content.is_empty())
        .ok_or_else(|| anyhow!("vision model returned no text"))
}

fn validate_image(mime_type: &str, bytes: &[u8]) -> anyhow::Result<()> {
    if bytes.is_empty() {
        bail!("photo is empty");
    }
    if bytes.len() > MAX_PHOTO_BYTES {
        bail!("photo is larger than 15 MB");
    }
    let valid = match mime_type {
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/webp" => bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        _ => false,
    };
    if !valid {
        bail!("use a JPEG, PNG, or WebP photo");
    }
    Ok(())
}

fn extension_for(mime_type: &str) -> anyhow::Result<&'static str> {
    match mime_type {
        "image/jpeg" => Ok("jpg"),
        "image/png" => Ok("png"),
        "image/webp" => Ok("webp"),
        _ => bail!("unsupported photo type"),
    }
}

fn pet_directory(config: &Config, household_id: i64, pet_id: i64) -> PathBuf {
    Path::new(&config.pet_photos_dir)
        .join(household_id.to_string())
        .join(pet_id.to_string())
}

fn photo_path(
    config: &Config,
    household_id: i64,
    pet_id: i64,
    storage_name: &str,
) -> PathBuf {
    pet_directory(config, household_id, pet_id).join(storage_name)
}

fn data_url(mime_type: &str, bytes: &[u8]) -> String {
    format!("data:{mime_type};base64,{}", STANDARD.encode(bytes))
}

fn strip_json_fence(value: &str) -> &str {
    let trimmed = value.trim();
    let trimmed = trimmed.strip_prefix("```json").unwrap_or(trimmed);
    let trimmed = trimmed.strip_prefix("```").unwrap_or(trimmed);
    trimmed.strip_suffix("```").unwrap_or(trimmed).trim()
}

fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_supported_magic_bytes() {
        assert!(validate_image("image/jpeg", &[0xff, 0xd8, 0xff, 0x00]).is_ok());
        assert!(validate_image("image/png", b"\x89PNG\r\n\x1a\nrest").is_ok());
        assert!(validate_image("image/webp", b"RIFF1234WEBPrest").is_ok());
        assert!(validate_image("image/jpeg", b"not-jpeg").is_err());
    }

    #[test]
    fn strips_json_code_fence() {
        assert_eq!(
            strip_json_fence("```json\n{\"found\":false}\n```"),
            "{\"found\":false}"
        );
    }
}
