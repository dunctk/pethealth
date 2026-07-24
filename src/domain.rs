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
    pub symptom: Option<SymptomObservation>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SymptomObservation {
    pub event_id: i64,
    pub episode_id: String,
    pub symptom: String,
    pub occurrence_count: Option<i64>,
    pub amount: Option<String>,
    pub contents: Option<String>,
    pub meal_relation: Option<String>,
    pub water_status: Option<String>,
    pub appetite_status: Option<String>,
    pub energy_status: Option<String>,
    pub pain_status: Option<String>,
    pub note: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MedicationAdministration {
    pub id: i64,
    pub pet_id: i64,
    pub pet_name: String,
    pub name: String,
    pub active_ingredient: Option<String>,
    pub dose_value: Option<f64>,
    pub dose_unit: Option<String>,
    pub route: Option<String>,
    pub administered_at: DateTime<Utc>,
    pub scheduled_at: Option<DateTime<Utc>>,
    pub status: String,
    pub raw_input: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MedicationPrescription {
    pub id: i64,
    pub pet_id: i64,
    pub pet_name: String,
    pub name: String,
    pub active_ingredient: Option<String>,
    pub concentration_value: Option<f64>,
    pub concentration_unit: Option<String>,
    pub dose_value: Option<f64>,
    pub dose_unit: Option<String>,
    pub frequency: Option<String>,
    pub route: Option<String>,
    pub instructions: Option<String>,
    pub started_on: Option<String>,
    pub ended_on: Option<String>,
    pub status: String,
    pub raw_input: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MedicationAdherence {
    pub id: i64,
    pub prescription_id: i64,
    pub pet_id: i64,
    pub pet_name: String,
    pub scheduled_for: String,
    pub expected_dose_value: Option<f64>,
    pub expected_dose_unit: Option<String>,
    pub actual_dose_value: Option<f64>,
    pub actual_dose_unit: Option<String>,
    pub status: String,
    pub reason: Option<String>,
    pub raw_input: Option<String>,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MedicationPlan {
    pub prescriptions: Vec<MedicationPrescription>,
    pub adherence: Vec<MedicationAdherence>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TemporalLink {
    pub event_id: i64,
    pub medication_id: i64,
    pub minutes_after_medication: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ClinicalTimeline {
    pub events: Vec<HealthEvent>,
    pub medications: Vec<MedicationAdministration>,
    pub prescriptions: Vec<MedicationPrescription>,
    pub adherence: Vec<MedicationAdherence>,
    pub temporal_links: Vec<TemporalLink>,
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

#[derive(Clone, Debug, Serialize)]
pub struct WeightEntry {
    pub id: i64,
    pub pet_id: i64,
    pub weight_kg: f64,
    pub measured_at: DateTime<Utc>,
    pub note: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LabReport {
    pub id: i64,
    pub pet_id: i64,
    pub pet_name: String,
    pub source_filename: String,
    pub raw_text: String,
    pub test_date: Option<String>,
    pub imported_at: DateTime<Utc>,
    pub parse_status: String,
    pub results: Vec<LabResult>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LabResult {
    pub id: i64,
    pub test_name: String,
    pub value_text: String,
    pub value_numeric: Option<f64>,
    pub unit: Option<String>,
    pub reference_range: Option<String>,
    pub flag: Option<String>,
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
