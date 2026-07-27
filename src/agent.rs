use crate::{config::Config, domain::ProposedEvent};
use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use rig_core::providers::openai;
use thiserror::Error;

#[derive(Clone)]
pub struct CaptureAgent {
    llm: Option<LlmConfig>,
}

#[derive(Clone, Debug)]
pub struct CaptureIntent {
    pub event: ProposedEvent,
    pub missed_medication: bool,
}

#[derive(Clone)]
struct LlmConfig {
    api_key: String,
    base_url: String,
    model: String,
}

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("Tell me which pet this is about.")]
    PetMissing,
    #[error("I found more than one possible pet. Please use the pet's full name.")]
    PetAmbiguous,
    #[error(
        "I couldn't understand that yet. Try “Milo vomited just now” or describe one thing that happened."
    )]
    Unsupported,
    #[error("The configured language model could not parse that observation.")]
    Model,
    #[error("The language model did not answer in time. Nothing was saved — try again.")]
    ModelTimeout,
}

/// How long to wait on the extraction call before giving up. `rig` builds its own
/// HTTP client with no timeout, so without this a stalled upstream leaves the
/// POST open indefinitely and the capture box sits on "SAVING..." forever with
/// nothing saved and no way to recover but a reload.
const MODEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

impl CaptureAgent {
    pub fn new(config: &Config) -> Self {
        Self {
            llm: config.llm_api_key.as_ref().map(|api_key| LlmConfig {
                api_key: api_key.clone(),
                base_url: config.llm_base_url.clone(),
                model: config.llm_model.clone(),
            }),
        }
    }

    pub async fn propose(
        &self,
        input: &str,
        pet_names: &[String],
    ) -> Result<ProposedEvent, CaptureError> {
        Ok(self.propose_capture(input, pet_names, None).await?.event)
    }

    pub async fn propose_capture(
        &self,
        input: &str,
        pet_names: &[String],
        selected_pet: Option<&str>,
    ) -> Result<CaptureIntent, CaptureError> {
        if let Some(intent) = deterministic_proposal(input, pet_names, selected_pet)? {
            return Ok(intent);
        }
        let Some(llm) = &self.llm else {
            return Err(CaptureError::Unsupported);
        };
        let client = openai::Client::builder()
            .api_key(&llm.api_key)
            .base_url(&llm.base_url)
            .build()
            .map_err(|_| CaptureError::Model)?;
        let prompt = format!(
            "Extract one factual pet-health event. Known pets: {}. Selected pet context: {}. Use the selected pet when the input uses she, he, or they without a name; otherwise use only a known pet name. \
             event_type must be one of observation, symptom, medication, measurement, vet_visit. \
             concept is a short lowercase canonical phrase. Preserve factual medication absence and appetite wording in details. Do not invent a medicine, dose, diagnosis, or timestamp. minutes_ago is only for explicit relative time. Input: {}",
            pet_names.join(", "),
            selected_pet.unwrap_or("none"),
            input
        );
        let proposal = tokio::time::timeout(
            MODEL_TIMEOUT,
            client
                .extractor::<ProposedEvent>(&llm.model)
                .build()
                .extract(&prompt),
        )
        .await
        .map_err(|_| CaptureError::ModelTimeout)?
        .map_err(|_| CaptureError::Model)?;
        validate_pet(&proposal.pet_name, pet_names)?;
        Ok(CaptureIntent {
            event: proposal,
            missed_medication: false,
        })
    }

    pub fn occurred_at(
        &self,
        proposal: &ProposedEvent,
        received_at: DateTime<Utc>,
    ) -> DateTime<Utc> {
        received_at - Duration::minutes(proposal.minutes_ago.unwrap_or(0).clamp(0, 525_600))
    }
}

fn deterministic_proposal(
    input: &str,
    pet_names: &[String],
    selected_pet: Option<&str>,
) -> Result<Option<CaptureIntent>, CaptureError> {
    let pet_name = resolve_pet(input, pet_names, selected_pet)?;
    let lower = input.to_lowercase();
    if mentions_missed_medication(&lower) && mentions_reasonable_appetite(&lower) {
        return Ok(Some(CaptureIntent {
            event: ProposedEvent {
                pet_name,
                event_type: "observation".into(),
                concept: "care_update".into(),
                summary: "Medication not given; appetite reasonable".into(),
                details: Some(
                    "No medication was given this morning. A reasonable amount was eaten.".into(),
                ),
                minutes_ago: None,
            },
            missed_medication: true,
        }));
    }
    let (event_type, concept, summary) = if contains_any(
        &lower,
        &["vomit", "vomited", "puked", "threw up", "sick was"],
    ) {
        ("symptom", "vomiting", "Vomited")
    } else if contains_any(&lower, &["diarrhea", "diarrhoea", "loose stool"]) {
        ("symptom", "diarrhea", "Had diarrhea")
    } else if contains_any(&lower, &["sneezed", "sneezing"]) {
        ("symptom", "sneezing", "Sneezed")
    } else if contains_any(&lower, &["not eating", "wouldn't eat", "refused food"]) {
        ("symptom", "reduced_appetite", "Did not eat")
    } else if contains_any(
        &lower,
        &["gave", "took medicine", "had medicine", "medication"],
    ) {
        ("medication", "medication_administered", "Medication given")
    } else {
        return Ok(None);
    };
    let minutes_ago = parse_minutes_ago(&lower);
    let details = occurrence_count(&lower).map(|count| format!("Reported count: {count}"));
    Ok(Some(CaptureIntent {
        event: ProposedEvent {
            pet_name,
            event_type: event_type.into(),
            concept: concept.into(),
            summary: summary.into(),
            details,
            minutes_ago,
        },
        missed_medication: false,
    }))
}

fn resolve_pet(
    input: &str,
    pet_names: &[String],
    selected_pet: Option<&str>,
) -> Result<String, CaptureError> {
    let lower = input.to_lowercase();
    let matches: Vec<_> = pet_names
        .iter()
        .filter(|name| {
            let escaped = regex::escape(&name.to_lowercase());
            Regex::new(&format!(
                r"(?:^|[^\p{{L}}\p{{N}}]){escaped}(?:$|[^\p{{L}}\p{{N}}])"
            ))
            .is_ok_and(|regex| regex.is_match(&lower))
        })
        .cloned()
        .collect();
    match matches.as_slice() {
        [name] => Ok(name.clone()),
        [] => selected_pet
            .filter(|selected| {
                pet_names
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(selected))
            })
            .map(str::to_owned)
            .ok_or(CaptureError::PetMissing),
        _ => Err(CaptureError::PetAmbiguous),
    }
}

fn validate_pet(name: &str, pet_names: &[String]) -> Result<(), CaptureError> {
    if pet_names
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(name))
    {
        Ok(())
    } else {
        Err(CaptureError::PetMissing)
    }
}

fn parse_minutes_ago(input: &str) -> Option<i64> {
    if input.contains("just now") || input.contains("right now") {
        return Some(0);
    }
    let regex = Regex::new(r"\b(\d{1,4})\s*(?:minute|minutes|min|mins)\s+ago\b").unwrap();
    regex
        .captures(input)
        .and_then(|capture| capture[1].parse().ok())
}

fn occurrence_count(input: &str) -> Option<u8> {
    if input.contains("twice") {
        Some(2)
    } else if input.contains("three times") {
        Some(3)
    } else {
        Regex::new(r"\b(\d{1,2})\s+times\b")
            .unwrap()
            .captures(input)
            .and_then(|capture| capture[1].parse().ok())
    }
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn mentions_missed_medication(input: &str) -> bool {
    let absence = contains_any(
        input,
        &[
            "no medication",
            "no medications",
            "no medicine",
            "no medicines",
            "no drugs",
            "without medication",
            "haven't given",
            "havent given",
            "have not given",
            "didn't give",
            "didnt give",
            "did not give",
        ],
    );
    absence
        && contains_any(
            input,
            &["med", "medicine", "medication", "drug", "pill", "tablet"],
        )
}

fn mentions_reasonable_appetite(input: &str) -> bool {
    contains_any(
        input,
        &[
            "eaten a reasonable amount",
            "ate a reasonable amount",
            "eating a reasonable amount",
            "eaten reasonably",
            "ate reasonably",
            "eating reasonably",
            "eaten well",
            "ate well",
            "eating well",
            "good appetite",
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn structures_milo_vomiting_without_a_model() {
        let agent = CaptureAgent { llm: None };
        let result = agent
            .propose("Milo vomited just now", &["Milo".into(), "Luna".into()])
            .await
            .unwrap();
        assert_eq!(result.pet_name, "Milo");
        assert_eq!(result.concept, "vomiting");
        assert_eq!(result.minutes_ago, Some(0));
    }

    #[tokio::test]
    async fn uses_selected_pet_and_records_compound_care_update_without_a_model() {
        let agent = CaptureAgent { llm: None };
        let result = agent
            .propose_capture(
                "we havent given her any drugs this morning, but she has eaten a reasonable amount",
                &["Milo".into()],
                Some("Milo"),
            )
            .await
            .unwrap();
        assert_eq!(result.event.pet_name, "Milo");
        assert_eq!(result.event.concept, "care_update");
        assert_eq!(result.event.event_type, "observation");
        assert!(result.missed_medication);
        assert!(
            result
                .event
                .details
                .as_deref()
                .unwrap()
                .contains("reasonable amount")
        );
    }

    #[tokio::test]
    async fn refuses_to_guess_a_pet() {
        let agent = CaptureAgent { llm: None };
        assert!(matches!(
            agent.propose("someone vomited", &["Milo".into()]).await,
            Err(CaptureError::PetMissing)
        ));
    }
}
