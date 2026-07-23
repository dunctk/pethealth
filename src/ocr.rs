use crate::{config::Config, db, domain::Pet};
use anyhow::anyhow;
use base64::Engine;
use regex::Regex;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct ParsedLabResult {
    pub test_name: String,
    pub value_text: String,
    pub value_numeric: Option<f64>,
    pub unit: Option<String>,
    pub reference_range: Option<String>,
    pub flag: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OcrResponse {
    pages: Vec<OcrPage>,
    #[allow(dead_code)]
    model: Option<String>,
}
#[derive(Debug, Deserialize)]
struct OcrPage {
    markdown: Option<String>,
    blocks: Option<Vec<OcrBlock>>,
}
#[derive(Debug, Deserialize)]
struct OcrBlock {
    content: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ImportResult {
    pub filename: String,
    pub report_id: Option<i64>,
    pub message: String,
}

pub fn household_directory(config: &Config, household_id: i64) -> anyhow::Result<PathBuf> {
    if household_id < 1 {
        return Err(anyhow!("household id must be positive"));
    }
    Ok(Path::new(&config.blood_tests_dir).join(household_id.to_string()))
}

pub async fn store_upload(
    config: &Config,
    household_id: i64,
    filename: &str,
    bytes: &[u8],
) -> anyhow::Result<String> {
    if bytes.is_empty() {
        return Err(anyhow!("the uploaded file is empty"));
    }
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Err(anyhow!("the uploaded file is too large"));
    }
    let safe_name = safe_filename(filename)?;
    let directory = household_directory(config, household_id)?;
    tokio::fs::create_dir_all(&directory).await?;
    let mut destination = directory.join(&safe_name);
    if tokio::fs::try_exists(&destination).await? {
        let hash = hex_hash(bytes);
        let stem = destination
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("blood-test");
        let extension = destination
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!(".{value}"))
            .unwrap_or_default();
        destination = directory.join(format!("{stem}-{}{extension}", &hash[..12]));
    }
    tokio::fs::write(&destination, bytes).await?;
    Ok(destination
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(&safe_name)
        .to_owned())
}

pub async fn import_directory(
    config: &Config,
    db: &sea_orm::DatabaseConnection,
    household_id: i64,
    actor: &str,
    pets: &[Pet],
) -> anyhow::Result<Vec<ImportResult>> {
    let path = household_directory(config, household_id)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut entries = tokio::fs::read_dir(path).await?;
    let mut results = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let file = entry.path();
        if !is_supported(&file) {
            continue;
        }
        let filename = file
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("document")
            .to_owned();
        let bytes = tokio::fs::read(&file).await?;
        let hash = hex_hash(&bytes);
        if db::report_hash_exists(db, household_id, &hash).await? {
            results.push(ImportResult {
                filename,
                report_id: None,
                message: "Already imported".into(),
            });
            continue;
        }
        let pet = match match_pet(&filename, pets) {
            Some(pet) => pet,
            None => {
                results.push(ImportResult {
                    filename,
                    report_id: None,
                    message: "Could not match this file to a pet".into(),
                });
                continue;
            }
        };
        let text = ocr_document(config, &bytes, &file).await?;
        let parsed = parse_results(&text);
        let test_date = find_date(&text);
        let report_id = db::create_lab_report(
            db,
            household_id,
            actor,
            pet.id,
            &filename,
            &hash,
            &text,
            test_date.as_deref(),
            &parsed,
        )
        .await?;
        results.push(ImportResult {
            filename,
            report_id: Some(report_id),
            message: format!("Imported for {}", pet.name),
        });
    }
    Ok(results)
}

async fn ocr_document(config: &Config, bytes: &[u8], path: &Path) -> anyhow::Result<String> {
    let key = config
        .mistral_api_key
        .as_deref()
        .ok_or_else(|| anyhow!("MISTRAL_API_KEY is not configured"))?;
    let mime = mime_for(path).ok_or_else(|| anyhow!("Unsupported blood-test file type"))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let document_type = if mime == "application/pdf" {
        "document_url"
    } else {
        "image_url"
    };
    let data_url = format!("data:{mime};base64,{encoded}");
    let document = if document_type == "document_url" {
        serde_json::json!({"type":document_type,"document_url":data_url})
    } else {
        serde_json::json!({"type":document_type,"image_url":data_url})
    };
    let response = reqwest::Client::new()
        .post("https://api.mistral.ai/v1/ocr")
        .bearer_auth(key)
        .json(&serde_json::json!({"model":"mistral-ocr-4-0","document":document,"include_blocks":true,"table_format":"markdown","extract_header":true,"extract_footer":true,"confidence_scores_granularity":"page"}))
        .send().await?.error_for_status()?.json::<OcrResponse>().await?;
    let mut pages = Vec::new();
    for page in response.pages {
        if let Some(markdown) = page.markdown {
            pages.push(markdown);
        }
        if let Some(blocks) = page.blocks {
            pages.extend(blocks.into_iter().filter_map(|block| block.content));
        }
    }
    Ok(pages.join("\n\n"))
}

fn is_supported(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|v| v.to_str())
            .map(|v| v.to_ascii_lowercase())
            .as_deref(),
        Some("pdf" | "png" | "jpg" | "jpeg" | "avif")
    )
}

fn safe_filename(filename: &str) -> anyhow::Result<String> {
    let basename = Path::new(filename)
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("a filename is required"))?;
    let cleaned = basename
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '.' | '-' | '_' | ' ') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim()
        .trim_matches('.')
        .to_owned();
    if cleaned.is_empty() || !is_supported(Path::new(&cleaned)) {
        return Err(anyhow!("only PDF and image blood-test files are supported"));
    }
    Ok(cleaned)
}
fn mime_for(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|v| v.to_str())
        .map(|v| v.to_ascii_lowercase())
        .as_deref()
    {
        Some("pdf") => Some("application/pdf"),
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("avif") => Some("image/avif"),
        _ => None,
    }
}
fn hex_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn match_pet(filename: &str, pets: &[Pet]) -> Option<Pet> {
    let lower = filename.to_lowercase();
    pets.iter()
        .find(|pet| lower.contains(&pet.name.to_lowercase()))
        .cloned()
        .or_else(|| (pets.len() == 1).then(|| pets[0].clone()))
}

fn is_metadata_name(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "id" | "sexo"
            | "sex"
            | "edad"
            | "age"
            | "fecha"
            | "date"
            | "fecha de registro"
            | "registration date"
    )
}

fn find_date(text: &str) -> Option<String> {
    let iso = Regex::new(r"\b(20\d{2}[-/]\d{1,2}[-/]\d{1,2})\b").unwrap();
    if let Some(found) = iso.captures(text).and_then(|c| c.get(1)) {
        return Some(found.as_str().replace('/', "-"));
    }
    Regex::new(r"\b(\d{1,2})[/.](\d{1,2})[/.](20\d{2})\b")
        .unwrap()
        .captures(text)
        .map(|c| format!("{}-{:0>2}-{:0>2}", &c[3], &c[2], &c[1]))
}

pub fn parse_results(text: &str) -> Vec<ParsedLabResult> {
    let mut output = Vec::new();
    let test_name = Regex::new(r"(?im)^(?:test|prueba)\s*:\s*(.+)$")
        .unwrap()
        .captures_iter(
            text.lines()
                .map(str::trim)
                .collect::<Vec<_>>()
                .join("\n")
                .as_str(),
        )
        .last()
        .and_then(|capture| capture.get(1).map(|value| value.as_str().trim().to_owned()))
        .unwrap_or_else(|| "Blood test result".into());
    if let Some(capture) = Regex::new(r"(?im)^(?:result|resultado)\s*:\s*(.+)$")
        .unwrap()
        .captures_iter(
            text.lines()
                .map(str::trim)
                .collect::<Vec<_>>()
                .join("\n")
                .as_str(),
        )
        .next()
    {
        let labeled_value = capture[1].trim();
        let number = Regex::new(r"[-+]?\d+(?:[.,]\d+)?")
            .unwrap()
            .find(labeled_value);
        let value_text = number
            .map(|number| number.as_str().to_owned())
            .unwrap_or_else(|| labeled_value.to_owned());
        let unit = number.and_then(|number| {
            let unit = labeled_value[number.end()..].trim();
            (!unit.is_empty()).then(|| unit.to_owned())
        });
        output.push(ParsedLabResult {
            test_name,
            value_numeric: parse_number(&value_text),
            value_text,
            unit,
            reference_range: None,
            flag: None,
        });
    }
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let cells: Vec<_> = line
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .filter(|cell| !cell.is_empty())
            .collect();
        if cells.len() < 2
            || cells.iter().any(|cell| {
                cell.chars()
                    .all(|c| c == '-' || c == ':' || c.is_whitespace())
            })
        {
            continue;
        }
        let numeric_index = cells.iter().position(|cell| parse_number(cell).is_some());
        let Some(index) = numeric_index else {
            continue;
        };
        let name = cells.first().unwrap().trim();
        if name.len() < 2
            || name.eq_ignore_ascii_case("test")
            || name.eq_ignore_ascii_case("prueba")
            || is_metadata_name(name)
        {
            continue;
        }
        let value_text = cells[index].to_owned();
        let unit = cells
            .get(index + 1)
            .filter(|value| value.len() < 20 && parse_number(value).is_none())
            .map(|value| (*value).to_owned());
        let reference_range = cells
            .iter()
            .find(|cell| {
                cell.contains('-')
                    && parse_number(cell.split('-').next().unwrap_or_default()).is_some()
            })
            .map(|value| (*value).to_owned());
        let flag = cells
            .iter()
            .find(|cell| {
                matches!(
                    cell.to_ascii_lowercase().as_str(),
                    "h" | "l" | "high" | "low" | "alto" | "bajo" | "*"
                )
            })
            .map(|value| (*value).to_owned());
        output.push(ParsedLabResult {
            test_name: name.to_owned(),
            value_text,
            value_numeric: parse_number(cells[index]),
            unit,
            reference_range,
            flag,
        });
    }
    output
}

fn parse_number(value: &str) -> Option<f64> {
    let cleaned = value.trim().replace(',', ".");
    Regex::new(r"[-+]?\d+(?:\.\d+)?")
        .unwrap()
        .find(&cleaned)
        .and_then(|m| m.as_str().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(root: &Path) -> Config {
        Config {
            host: "127.0.0.1".into(),
            port: 3000,
            username: "owner".into(),
            password: "password".into(),
            production: false,
            database_url: "sqlite::memory:".into(),
            llm_api_key: None,
            llm_base_url: String::new(),
            llm_model: String::new(),
            mistral_api_key: None,
            blood_tests_dir: root.to_string_lossy().into_owned(),
        }
    }
    #[test]
    fn parses_english_and_spanish_table_rows() {
        let results = parse_results(
            "| Prueba | Resultado | Unidad |\n| ID | 19072 | |\n| Hemoglobina | 14,2 | g/dL |\n| ALT | 55 | U/L | H |",
        );
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].test_name, "Hemoglobina");
        assert_eq!(results[0].value_numeric, Some(14.2));
    }

    #[test]
    fn parses_labeled_single_result() {
        let results = parse_results("Test : T4\nResult : 44.72 nmol/L\nDate : 2022-05-06");
        assert_eq!(results[0].test_name, "T4");
        assert_eq!(results[0].value_text, "44.72");
        assert_eq!(results[0].value_numeric, Some(44.72));
        assert_eq!(results[0].unit.as_deref(), Some("nmol/L"));
    }

    #[tokio::test]
    async fn uploaded_files_are_separated_by_household() {
        let root = tempfile::tempdir().unwrap();
        let config = test_config(root.path());
        store_upload(&config, 1, "../sample.pdf", b"first")
            .await
            .unwrap();
        store_upload(&config, 2, "sample.pdf", b"second")
            .await
            .unwrap();

        assert_eq!(
            tokio::fs::read(root.path().join("1/sample.pdf"))
                .await
                .unwrap(),
            b"first"
        );
        assert_eq!(
            tokio::fs::read(root.path().join("2/sample.pdf"))
                .await
                .unwrap(),
            b"second"
        );
    }
}
