use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const DEFAULT_HOUSEHOLD_ID: i64 = 1;

#[derive(Clone, Debug, Serialize)]
pub struct UserAccount {
    pub id: i64,
    pub household_id: i64,
    pub username: String,
    pub email: Option<String>,
    pub display_name: String,
    pub initials: String,
}

impl UserAccount {
    pub fn audit_actor(&self) -> String {
        format!("user:{}", self.id)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Pet {
    pub id: i64,
    pub name: String,
    pub species: String,
    pub breed: Option<String>,
    pub date_of_birth: Option<String>,
    pub weight_kg: Option<f64>,
    pub initials: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct HealthEvent {
    pub id: i64,
    pub pet_id: i64,
    pub pet_name: String,
    pub event_type: String,
    pub concept: String,
    pub summary: String,
    pub raw_input: String,
    pub details: Option<String>,
    pub occurred_at: DateTime<Utc>,
    pub recorded_at: DateTime<Utc>,
    pub source: String,
    pub status: String,
    pub occurred_label: String,
    pub icon: &'static str,
    pub tone: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct KnowledgeArticle {
    pub concept: String,
    pub title: String,
    pub summary: String,
    pub monitoring: String,
    pub urgent_signs: String,
    pub source_url: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ShareGrant {
    pub id: i64,
    pub household_id: i64,
    pub pet_id: i64,
    pub pet_name: String,
    pub label: String,
    pub token: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub status: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProposedEvent {
    pub pet_name: String,
    pub event_type: String,
    pub concept: String,
    pub summary: String,
    pub details: Option<String>,
    pub minutes_ago: Option<i64>,
}

pub fn event_presentation(event_type: &str, concept: &str) -> (&'static str, &'static str) {
    match (event_type, concept) {
        ("symptom", "vomiting") => ("↗", "warning"),
        ("symptom", _) => ("!", "warning"),
        ("medication", _) => ("+", "info"),
        ("measurement", _) => ("≈", "active"),
        ("vet_visit", _) => ("✚", "info"),
        _ => ("·", "neutral"),
    }
}
