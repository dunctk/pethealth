use crate::{config::Config, db, domain::Pet};
use anyhow::anyhow;
use base64::Engine;
use regex::Regex;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::Path;

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

pub async fn import_directory(
    config: &Config,
    db: &sea_orm::DatabaseConnection,
    household_id: i64,
    actor: &str,
    pets: &[Pet],
) -> anyhow::Result<Vec<ImportResult>> {
    let path = Path::new(&config.blood_tests_dir);
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
    #[test]
    fn parses_english_and_spanish_table_rows() {
        let results = parse_results(
            "| Prueba | Resultado | Unidad |\n| Hemoglobina | 14,2 | g/dL |\n| ALT | 55 | U/L | H |",
        );
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].test_name, "Hemoglobina");
        assert_eq!(results[0].value_numeric, Some(14.2));
    }
}
