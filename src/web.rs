use crate::{
    AppState,
    agent::ChatReply,
    auth, db,
    domain::{
        HealthEvent, KnowledgeArticle, LabReport, MedicationAdherence, MedicationAdministration,
        MedicationPlanChange, MedicationPrescription, Pet, ShareGrant, TimelineEntry, UserAccount,
        WeightEntry,
    },
    ocr,
};
use askama::Template;
use axum::{
    Extension, Form, Router,
    extract::{DefaultBodyLimit, Multipart, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Deserializer, Serialize};

const CSS: &str = include_str!("../static/app.css");

pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/app", get(index))
        .route("/app/tab/{view}", get(tab_fragment))
        .route("/app/timeline/older", get(timeline_older))
        .route("/app/forms/weight", get(record_form_weight))
        .route("/app/forms/dose", get(record_form_dose))
        .route("/app/forms/symptom", get(record_form_symptom))
        .route("/app/forms/lab", get(record_form_lab))
        .route("/app/events/{id}/drawer", get(event_drawer))
        .route("/pets", post(create_pet))
        .route("/weights", post(create_weight))
        .route("/symptoms", post(create_symptom))
        .route("/medications", post(create_medication))
        .route("/prescriptions", post(create_prescription))
        .route("/medication-adherence", post(create_adherence))
        .route("/blood-tests/upload", post(upload_blood_test))
        .route("/blood-tests/import", post(import_blood_tests))
        .route("/agent/capture", post(capture))
        .route("/agent/chat", post(agent_chat))
        .route(
            "/agent/medication-plan/confirm",
            post(confirm_agent_medication_plan),
        )
        .route("/events/{id}/undo", post(undo_event))
        .route("/events/{id}/summary", post(update_event_summary))
        .route("/shares", post(create_share))
        .route("/shares/{id}/revoke", post(revoke_share))
        .route("/account", get(account_page))
        .route("/account/password", post(change_password))
        .route("/logout", post(logout))
        .layer(DefaultBodyLimit::max(ocr::MAX_UPLOAD_BYTES))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));
    Router::new()
        .route("/", get(home_page))
        .route("/healthz", get(healthz))
        .route("/favicon.ico", get(favicon))
        .route("/static/app.css", get(css))
        .route("/static/app.js", get(js))
        .route("/login", get(login_page).post(login))
        .route("/register", get(register_page).post(register))
        .route("/share/{token}", get(shared_pet))
        .route(
            "/mcp",
            post(crate::mcp::endpoint)
                .layer(DefaultBodyLimit::max(ocr::MAX_UPLOAD_BYTES + 1024 * 1024)),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(crate::mcp::protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(crate::mcp::authorization_server_metadata),
        )
        .route("/oauth/register", post(crate::mcp::register_client))
        .route(
            "/oauth/device",
            get(crate::mcp::device_page).post(crate::mcp::start_device_authorization),
        )
        .route("/oauth/device/verify", post(crate::mcp::verify_device_code))
        .route(
            "/oauth/device/approve",
            post(crate::mcp::approve_device_code),
        )
        .route("/oauth/authorize", get(crate::mcp::authorize))
        .route(
            "/oauth/authorize/approve",
            post(crate::mcp::approve_authorize),
        )
        .route("/oauth/token", post(crate::mcp::token))
        .merge(protected)
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

#[derive(Deserialize, Default)]
struct LoginPageQuery {
    changed: Option<bool>,
    next: Option<String>,
}

async fn login_page(Query(query): Query<LoginPageQuery>) -> Result<Html<String>, AppError> {
    render(&LoginTemplate {
        identifier: String::new(),
        error: None,
        notice: query
            .changed
            .unwrap_or(false)
            .then(|| "Password updated. Sign in again on this device.".into()),
        next: query.next,
    })
}

async fn home_page(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Html<String>, AppError> {
    let origin = request_origin(&state, &headers);
    render(&HomeTemplate {
        mcp_url: format!("{origin}/mcp"),
        device_url: format!("{origin}/oauth/device"),
    })
}

async fn register_page() -> Result<Html<String>, AppError> {
    render(&RegisterTemplate {
        display_name: String::new(),
        email: String::new(),
        error: None,
    })
}

#[derive(Deserialize)]
struct LoginForm {
    identifier: String,
    password: String,
    next: Option<String>,
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Result<Response, AppError> {
    require_form_origin(&state, &headers)?;
    let identifier = clean_required(&form.identifier, 254, "Email or username")?.to_owned();
    let valid = if let Some((user, hash)) = db::user_for_login(&state.db, &identifier).await? {
        auth::verify_password(form.password, hash)
            .await
            .then_some(user)
    } else {
        None
    };
    let Some(user) = valid else {
        return render_status(
            &LoginTemplate {
                identifier,
                error: Some("Email/username or password is incorrect.".into()),
                notice: None,
                next: form.next,
            },
            StatusCode::UNPROCESSABLE_ENTITY,
        );
    };
    let token = db::create_session(&state.db, user.id).await?;
    let destination = safe_login_next(form.next.as_deref()).unwrap_or("/app");
    Ok(session_redirect(
        destination,
        session_cookie(&state, &token, false),
    ))
}

#[derive(Deserialize)]
struct RegisterForm {
    display_name: String,
    email: String,
    password: String,
}

async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<RegisterForm>,
) -> Result<Response, AppError> {
    require_form_origin(&state, &headers)?;
    let display_name = clean_required(&form.display_name, 80, "Name")?.to_owned();
    let email = normalize_email(&form.email)?;
    if form.password.chars().count() < 12 || form.password.chars().count() > 128 {
        return render_status(
            &RegisterTemplate {
                display_name,
                email,
                error: Some("Use a password between 12 and 128 characters.".into()),
            },
            StatusCode::UNPROCESSABLE_ENTITY,
        );
    }
    if db::user_for_login(&state.db, &email).await?.is_some() {
        return render_status(
            &RegisterTemplate {
                display_name,
                email,
                error: Some("An account with that email already exists.".into()),
            },
            StatusCode::UNPROCESSABLE_ENTITY,
        );
    }
    let password_hash = auth::hash_password(form.password).await?;
    let user = db::create_account(&state.db, &email, &display_name, &password_hash).await?;
    let token = db::create_session(&state.db, user.id).await?;
    Ok(session_redirect(
        "/app",
        session_cookie(&state, &token, false),
    ))
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, AppError> {
    if let Some(token) = session_token(&state, &headers) {
        db::revoke_session(&state.db, token).await?;
    }
    Ok(session_redirect("/login", session_cookie(&state, "", true)))
}

async fn account_page(Extension(user): Extension<UserAccount>) -> Result<Html<String>, AppError> {
    render(&AccountTemplate { user, error: None })
}

#[derive(Deserialize)]
struct PasswordForm {
    current_password: String,
    new_password: String,
}

async fn change_password(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Form(form): Form<PasswordForm>,
) -> Result<Response, AppError> {
    let current_valid =
        if let Some((_, hash)) = db::user_for_login(&state.db, &user.username).await? {
            auth::verify_password(form.current_password, hash).await
        } else {
            false
        };
    if !current_valid {
        return render_status(
            &AccountTemplate {
                user,
                error: Some("Current password is incorrect.".into()),
            },
            StatusCode::UNPROCESSABLE_ENTITY,
        );
    }
    if form.new_password.chars().count() < 12 || form.new_password.chars().count() > 128 {
        return render_status(
            &AccountTemplate {
                user,
                error: Some("Use a new password between 12 and 128 characters.".into()),
            },
            StatusCode::UNPROCESSABLE_ENTITY,
        );
    }
    let password_hash = auth::hash_password(form.new_password).await?;
    db::update_password_and_revoke_sessions(&state.db, &user, &password_hash).await?;
    Ok(session_redirect(
        "/login?changed=true",
        session_cookie(&state, "", true),
    ))
}

#[derive(Deserialize, Default)]
struct IndexQuery {
    pet: Option<i64>,
    view: Option<String>,
}

/// Which tab of the console is showing. Phase 1 only: each tab is rendered from
/// its own template struct carrying only the data that tab needs, so `index`
/// stops running all nine queries on every load. See `UI_REDESIGN_PLAN.md` §4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConsoleView {
    Timeline,
    Plan,
    Labs,
    Sharing,
}

impl ConsoleView {
    /// Unknown or missing values fall back to the timeline rather than erroring.
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("plan") => Self::Plan,
            Some("labs") => Self::Labs,
            Some("sharing") => Self::Sharing,
            _ => Self::Timeline,
        }
    }
    fn is_timeline(self) -> bool {
        self == Self::Timeline
    }
    fn is_plan(self) -> bool {
        self == Self::Plan
    }
    fn is_labs(self) -> bool {
        self == Self::Labs
    }
    fn is_sharing(self) -> bool {
        self == Self::Sharing
    }
    fn label(self) -> &'static str {
        match self {
            Self::Timeline => "Timeline",
            Self::Plan => "Plan",
            Self::Labs => "Labs",
            Self::Sharing => "Sharing",
        }
    }
    fn key(self) -> &'static str {
        match self {
            Self::Timeline => "timeline",
            Self::Plan => "plan",
            Self::Labs => "labs",
            Self::Sharing => "sharing",
        }
    }
}

/// How many timeline entries a page shows. `render_tab_from_data` and the
/// `/app/timeline/older` fragment both request `TIMELINE_PAGE_SIZE + 1` from
/// `db::list_timeline` and hand the result to `paginate_timeline`, which is the
/// cheapest way to know whether a further page exists without guessing from a
/// full page happening to come back exactly full.
const TIMELINE_PAGE_SIZE: u64 = 20;

/// One calendar-day group of the merged timeline (`UI_REDESIGN_PLAN.md` §3's
/// `── 25 Jul ──` headers). `label` is `None` for a group that continues a day
/// already headed on an earlier page — see `group_timeline_by_day` — so
/// "Load older" never prints a duplicate date header for the day it split on.
struct TimelineDay {
    label: Option<String>,
    entries: Vec<TimelineEntry>,
}

/// Keyset cursor for "Load older", pre-formatted as a ready-to-append,
/// already-percent-encoded query string fragment so the template never touches
/// a timestamp directly.
struct TimelineCursor {
    query: String,
}

impl TimelineCursor {
    fn new((at, id): (DateTime<Utc>, i64)) -> Self {
        Self {
            query: format!(
                "before_at={}&before_id={id}",
                urlencoding::encode(&at.to_rfc3339())
            ),
        }
    }
}

/// Splits a batch fetched as `TIMELINE_PAGE_SIZE + 1` rows into the page to
/// display plus, if that extra row came back, the cursor for the next page.
/// Fetching one row past the page size is enough to answer "is there more"
/// exactly (see `db::list_timeline`'s doc comment on the federated top-k bound
/// this relies on) instead of guessing from a full page happening to come back
/// exactly full, which would show a "Load older" button that dead-ends once.
fn paginate_timeline(
    mut entries: Vec<TimelineEntry>,
    page_size: u64,
) -> (Vec<TimelineEntry>, Option<TimelineCursor>) {
    let page_size = page_size as usize;
    if entries.len() > page_size {
        entries.truncate(page_size);
        let cursor = entries
            .last()
            .map(|entry| TimelineCursor::new(entry.sort_key()));
        (entries, cursor)
    } else {
        (entries, None)
    }
}

/// Groups an already time-descending page of entries into calendar-day blocks.
/// `continue_date`, when it matches the first entry's date, suppresses that
/// first group's header: the caller is `/app/timeline/older` continuing a page
/// whose last-shown day was already headed before this batch was appended.
fn group_timeline_by_day(
    entries: Vec<TimelineEntry>,
    continue_date: Option<NaiveDate>,
) -> Vec<TimelineDay> {
    let mut days: Vec<TimelineDay> = Vec::new();
    let mut current_date: Option<NaiveDate> = None;
    for entry in entries {
        let date = entry.sort_key().0.date_naive();
        if current_date != Some(date) {
            let label = if days.is_empty() && continue_date == Some(date) {
                None
            } else {
                Some(date.format("%d %b").to_string())
            };
            days.push(TimelineDay {
                label,
                entries: Vec::new(),
            });
            current_date = Some(date);
        }
        days.last_mut()
            .expect("a group was just pushed above")
            .entries
            .push(entry);
    }
    days
}

/// Data shared across the pet-header metrics strip and, when the matching tab is
/// selected, reused directly by that tab instead of being queried twice.
struct ConsoleData {
    events: Vec<HealthEvent>,
    weights: Vec<WeightEntry>,
    shares: Vec<ShareGrant>,
    prescriptions: Vec<MedicationPrescription>,
}

async fn load_console_data(
    state: &AppState,
    household_id: i64,
    pet_id: i64,
) -> Result<ConsoleData, AppError> {
    let events = db::list_events(&state.db, household_id, Some(pet_id), 50).await?;
    let weights = db::list_weights(&state.db, household_id, pet_id).await?;
    let shares = db::list_shares(&state.db, household_id).await?;
    let prescriptions = db::list_prescriptions(&state.db, household_id, pet_id, 20).await?;
    Ok(ConsoleData {
        events,
        weights,
        shares,
        prescriptions,
    })
}

/// Runs only the extra queries the selected tab needs on top of `ConsoleData`
/// (already loaded for the pet-header metrics strip) and renders that tab's
/// fragment to a string. Used both by the full `/app` page load and by the
/// `/app/tab/{view}` HTMX fragment route, so a full page load and an in-app tab
/// switch always agree on markup.
async fn render_tab_from_data(
    state: &AppState,
    household_id: i64,
    pet: &Pet,
    view: ConsoleView,
    data: ConsoleData,
) -> Result<String, AppError> {
    let html = match view {
        ConsoleView::Timeline => {
            // The old knowledge card (with `related_count`) that used to live
            // here, scoped to only the most recent event, is gone — Phase 4
            // replaced it with a per-entry drawer (`event_drawer`) opened from
            // whichever entry the visitor actually clicks.
            let raw_entries = db::list_timeline(
                &state.db,
                household_id,
                pet.id,
                None,
                TIMELINE_PAGE_SIZE + 1,
            )
            .await?;
            let (entries, next_cursor) = paginate_timeline(raw_entries, TIMELINE_PAGE_SIZE);
            let entries_count = entries.len();
            TabTimelineTemplate {
                pet_id: pet.id,
                days: group_timeline_by_day(entries, None),
                entries_count,
                next_cursor,
                load_more_oob: false,
            }
            .render()?
        }
        ConsoleView::Plan => {
            let adherence = db::list_adherence(&state.db, household_id, pet.id, 30).await?;
            let medications = db::list_medications(&state.db, household_id, pet.id, 20).await?;
            let active_prescriptions = data
                .prescriptions
                .iter()
                .filter(|prescription| prescription.status == "active")
                .cloned()
                .collect();
            TabPlanTemplate {
                pet: pet.clone(),
                prescriptions: data.prescriptions,
                active_prescriptions,
                adherence,
                medications,
                weights: data.weights,
            }
            .render()?
        }
        ConsoleView::Labs => {
            let lab_reports = db::list_lab_reports(&state.db, household_id, pet.id).await?;
            TabLabsTemplate {
                pet: pet.clone(),
                lab_reports,
            }
            .render()?
        }
        ConsoleView::Sharing => TabSharingTemplate {
            pet: pet.clone(),
            shares: data.shares,
            new_share_path: None,
        }
        .render()?,
    };
    Ok(html)
}

async fn index(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Query(query): Query<IndexQuery>,
) -> Result<Html<String>, AppError> {
    let pets = db::list_pets(&state.db, user.household_id).await?;
    let selected_pet = match query.pet.or_else(|| pets.first().map(|pet| pet.id)) {
        Some(id) => db::get_pet(&state.db, user.household_id, id).await?,
        None => None,
    };
    let view = ConsoleView::parse(query.view.as_deref());
    let mut tab_html = String::new();
    let mut assistant_html = String::new();
    let mut events_count = 0;
    let mut latest_weight = None;
    let mut shares_count = 0;
    let mut active_prescription_count = 0;
    if let Some(pet) = &selected_pet {
        let data = load_console_data(&state, user.household_id, pet.id).await?;
        events_count = data.events.len();
        latest_weight = data.weights.first().cloned();
        shares_count = data.shares.len();
        active_prescription_count = data
            .prescriptions
            .iter()
            .filter(|item| item.status == "active")
            .count();
        assistant_html = render_assistant_workbench(pet, view, "record", Vec::new(), None, None)?;
        tab_html = render_tab_from_data(&state, user.household_id, pet, view, data).await?;
    }
    render(&ConsoleTemplate {
        user,
        pets,
        selected_pet,
        view,
        tab_html,
        assistant_html,
        events_count,
        latest_weight,
        shares_count,
        active_prescription_count,
    })
}

/// `GET /app/tab/{view}` — the HTMX fragment counterpart of `index`. Returns just
/// the tab body so switching tabs is a bounded swap, not a full page reload.
/// Household-scoped via `db::get_pet(household_id, pet_id)`: the pet id from the
/// query string is never trusted without that check.
async fn tab_fragment(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Path(view): Path<String>,
    Query(query): Query<IndexQuery>,
) -> Result<Html<String>, AppError> {
    let pet_id = query.pet.ok_or_else(AppError::not_found)?;
    let pet = db::get_pet(&state.db, user.household_id, pet_id)
        .await?
        .ok_or_else(AppError::not_found)?;
    let view = ConsoleView::parse(Some(&view));
    let data = load_console_data(&state, user.household_id, pet.id).await?;
    let html = render_tab_from_data(&state, user.household_id, &pet, view, data).await?;
    Ok(Html(html))
}

/// `true` for an `hx-get`/`hx-post` request (htmx always sends this header),
/// `false` for a plain browser navigation or form submission. Every
/// `+ Record` fragment route and dialog-form POST branches on this: htmx
/// requests get the small fragment meant for a `<dialog>` or an
/// out-of-band swap, anything else gets a full, standalone response so the
/// same route works with the dialog system turned off (`UI_REDESIGN_PLAN.md`
/// §4A's progressive-enhancement constraint).
fn wants_fragment(headers: &HeaderMap) -> bool {
    headers
        .get("hx-request")
        .is_some_and(|value| value == "true")
}

#[derive(Deserialize)]
struct PetQuery {
    pet: i64,
}

/// Wraps a `+ Record` dialog form (or the drawer body) for direct, non-htmx
/// navigation: the fragment routes this feeds are built to render *just* the
/// form/detail partial for `hx-get`, but hitting the same URL directly in a
/// browser must still produce a usable page, not a bare, unstyled `<form>`
/// dropped in with no chrome (`UI_REDESIGN_PLAN.md` §4A).
fn standalone_or_fragment(
    headers: &HeaderMap,
    title: &str,
    pet: &Pet,
    body: String,
) -> Result<Html<String>, AppError> {
    if wants_fragment(headers) {
        Ok(Html(body))
    } else {
        render(&StandaloneTemplate {
            title: title.to_owned(),
            pet_id: pet.id,
            pet_name: pet.name.clone(),
            body,
        })
    }
}

/// `GET /app/forms/weight` — `+ Record → Weight`. Household-scoped via
/// `db::get_pet`; the `pet` query-string value is never trusted otherwise.
async fn record_form_weight(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Query(query): Query<PetQuery>,
    headers: HeaderMap,
) -> Result<Html<String>, AppError> {
    let pet = db::get_pet(&state.db, user.household_id, query.pet)
        .await?
        .ok_or_else(AppError::not_found)?;
    let body = FormWeightTemplate { pet: pet.clone() }.render()?;
    standalone_or_fragment(&headers, "Add weight", &pet, body)
}

/// `GET /app/forms/dose` — `+ Record → Dose`. Household-scoped via `db::get_pet`.
async fn record_form_dose(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Query(query): Query<PetQuery>,
    headers: HeaderMap,
) -> Result<Html<String>, AppError> {
    let pet = db::get_pet(&state.db, user.household_id, query.pet)
        .await?
        .ok_or_else(AppError::not_found)?;
    let body = FormDoseTemplate { pet: pet.clone() }.render()?;
    standalone_or_fragment(&headers, "Log a dose", &pet, body)
}

/// `GET /app/forms/symptom` — `+ Record → Symptom`. This is the structured
/// symptom form migrated out of its old always-on `<details>` in
/// `_agent_timeline.html` (`UI_REDESIGN_PLAN.md` §4A migration table).
/// Household-scoped via `db::get_pet`.
async fn record_form_symptom(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Query(query): Query<PetQuery>,
    headers: HeaderMap,
) -> Result<Html<String>, AppError> {
    let pet = db::get_pet(&state.db, user.household_id, query.pet)
        .await?
        .ok_or_else(AppError::not_found)?;
    let body = FormSymptomTemplate { pet: pet.clone() }.render()?;
    standalone_or_fragment(&headers, "Structured symptom record", &pet, body)
}

/// `GET /app/forms/lab` — `+ Record → Lab`. The same upload form that lives
/// permanently in the Labs tab, also reachable as a dialog shortcut.
/// Household-scoped via `db::get_pet`.
async fn record_form_lab(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Query(query): Query<PetQuery>,
    headers: HeaderMap,
) -> Result<Html<String>, AppError> {
    let pet = db::get_pet(&state.db, user.household_id, query.pet)
        .await?
        .ok_or_else(AppError::not_found)?;
    let body = FormLabTemplate { pet: pet.clone() }.render()?;
    standalone_or_fragment(&headers, "Upload a blood test", &pet, body)
}

/// `GET /app/events/{id}/drawer` — the transient knowledge drawer opened from
/// a Timeline entry (`UI_REDESIGN_PLAN.md` §4B). Only `TimelineEntry::Event`
/// rows are clickable for this: they are the one timeline source with a
/// `concept` a knowledge article can be looked up by. Household- *and*
/// pet-scoped: `db::get_pet` first, then `db::get_event(household_id, pet.id,
/// event_id)` so an event id from the query string can never resolve another
/// household's (or another pet's) event.
async fn event_drawer(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Path(event_id): Path<i64>,
    Query(query): Query<PetQuery>,
    headers: HeaderMap,
) -> Result<Html<String>, AppError> {
    let pet = db::get_pet(&state.db, user.household_id, query.pet)
        .await?
        .ok_or_else(AppError::not_found)?;
    let event = db::get_event(&state.db, user.household_id, pet.id, event_id)
        .await?
        .ok_or_else(AppError::not_found)?;
    let related_count =
        db::count_related(&state.db, user.household_id, pet.id, &event.concept).await?;
    let knowledge = db::get_knowledge(&state.db, &event.concept).await?;
    let body = DrawerTemplate {
        event,
        knowledge,
        related_count,
    }
    .render()?;
    standalone_or_fragment(&headers, "Entry detail", &pet, body)
}

#[derive(Deserialize)]
struct PetForm {
    name: String,
    species: String,
    breed: Option<String>,
    weight_kg: Option<f64>,
}

async fn create_pet(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Form(form): Form<PetForm>,
) -> Result<Redirect, AppError> {
    let name = clean_required(&form.name, 80, "Pet name")?;
    let species = clean_required(&form.species, 40, "Species")?;
    let breed = clean_optional(form.breed.as_deref(), 80);
    if form
        .weight_kg
        .is_some_and(|weight| !(0.01..=500.0).contains(&weight))
    {
        return Err(AppError::validation(
            "Weight must be between 0.01 and 500 kg.",
        ));
    }
    let id = db::create_pet(
        &state.db,
        user.household_id,
        &user.audit_actor(),
        name,
        species,
        breed,
        form.weight_kg,
    )
    .await?;
    Ok(Redirect::to(&format!("/app?pet={id}")))
}

#[derive(Deserialize)]
struct WeightForm {
    pet_id: i64,
    weight_kg: f64,
    measured_at: String,
    note: Option<String>,
}

async fn create_weight(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    headers: HeaderMap,
    Form(form): Form<WeightForm>,
) -> Result<Response, AppError> {
    let pet = db::get_pet(&state.db, user.household_id, form.pet_id)
        .await?
        .ok_or_else(AppError::not_found)?;
    if !(0.01..=500.0).contains(&form.weight_kg) {
        return Err(AppError::validation(
            "Weight must be between 0.01 and 500 kg.",
        ));
    }
    let date = clean_required(&form.measured_at, 30, "Date")?;
    let measured_at = if date.len() == 10 {
        format!("{date}T12:00:00Z")
    } else {
        date.to_owned()
    };
    db::create_weight(
        &state.db,
        user.household_id,
        &user.audit_actor(),
        form.pet_id,
        form.weight_kg,
        &measured_at,
        clean_optional(form.note.as_deref(), 240),
    )
    .await?;
    timeline_write_response(&state, &headers, user.household_id, pet, true).await
}

#[derive(Deserialize)]
struct SymptomForm {
    pet_id: i64,
    symptom: String,
    raw_input: String,
    #[serde(default, deserialize_with = "empty_str_as_none")]
    occurrence_count: Option<i64>,
    amount: Option<String>,
    contents: Option<String>,
    meal_relation: Option<String>,
    water_status: Option<String>,
    appetite_status: Option<String>,
    energy_status: Option<String>,
    pain_status: Option<String>,
    note: Option<String>,
}

async fn create_symptom(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    headers: HeaderMap,
    Form(form): Form<SymptomForm>,
) -> Result<Response, AppError> {
    let pet = db::get_pet(&state.db, user.household_id, form.pet_id)
        .await?
        .ok_or_else(AppError::not_found)?;
    let symptom = clean_required(&form.symptom, 60, "Symptom")?;
    if !matches!(symptom, "vomiting" | "diarrhea" | "reduced_appetite") {
        return Err(AppError::validation("Choose a supported symptom."));
    }
    let count = form
        .occurrence_count
        .filter(|value| (1..=100).contains(value));
    let raw_input = clean_required(&form.raw_input, 1000, "Original wording")?;
    db::create_symptom_event(
        &state.db,
        user.household_id,
        &user.audit_actor(),
        &pet,
        raw_input,
        Utc::now(),
        symptom,
        count,
        clean_optional(form.amount.as_deref(), 60),
        clean_optional(form.contents.as_deref(), 120),
        clean_optional(form.meal_relation.as_deref(), 60),
        clean_optional(form.water_status.as_deref(), 60),
        clean_optional(form.appetite_status.as_deref(), 60),
        clean_optional(form.energy_status.as_deref(), 60),
        clean_optional(form.pain_status.as_deref(), 60),
        clean_optional(form.note.as_deref(), 500),
        "owner_form",
    )
    .await?;
    timeline_write_response(&state, &headers, user.household_id, pet, false).await
}

#[derive(Deserialize)]
struct MedicationForm {
    pet_id: i64,
    name: String,
    active_ingredient: Option<String>,
    #[serde(default, deserialize_with = "empty_str_as_none")]
    dose_value: Option<f64>,
    dose_unit: Option<String>,
    route: Option<String>,
    status: Option<String>,
    note: Option<String>,
}

async fn create_medication(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    headers: HeaderMap,
    Form(form): Form<MedicationForm>,
) -> Result<Response, AppError> {
    let pet = db::get_pet(&state.db, user.household_id, form.pet_id)
        .await?
        .ok_or_else(AppError::not_found)?;
    let name = clean_required(&form.name, 120, "Medication")?;
    let status = clean_optional(form.status.as_deref(), 30).unwrap_or("given");
    if !matches!(status, "given" | "missed" | "extra" | "vomited_back") {
        return Err(AppError::validation(
            "Choose a supported medication status.",
        ));
    }
    db::create_medication_administration(
        &state.db,
        user.household_id,
        &user.audit_actor(),
        &pet,
        name,
        clean_optional(form.active_ingredient.as_deref(), 120),
        form.dose_value,
        clean_optional(form.dose_unit.as_deref(), 30),
        clean_optional(form.route.as_deref(), 40),
        Utc::now(),
        None,
        status,
        clean_optional(form.note.as_deref(), 500),
    )
    .await?;
    timeline_write_response(&state, &headers, user.household_id, pet, false).await
}

#[derive(Deserialize)]
struct PrescriptionForm {
    pet_id: i64,
    name: String,
    active_ingredient: Option<String>,
    concentration_value: Option<f64>,
    concentration_unit: Option<String>,
    dose_value: Option<f64>,
    dose_unit: Option<String>,
    frequency: Option<String>,
    route: Option<String>,
    instructions: Option<String>,
    started_on: Option<String>,
    status: Option<String>,
    raw_input: Option<String>,
}

async fn create_prescription(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Form(form): Form<PrescriptionForm>,
) -> Result<Redirect, AppError> {
    let pet = db::get_pet(&state.db, user.household_id, form.pet_id)
        .await?
        .ok_or_else(AppError::not_found)?;
    let name = clean_required(&form.name, 120, "Medication")?;
    let status = clean_optional(form.status.as_deref(), 30).unwrap_or("active");
    if !matches!(status, "active" | "paused" | "stopped") {
        return Err(AppError::validation(
            "Choose a supported prescription status.",
        ));
    }
    db::create_medication_prescription(
        &state.db,
        user.household_id,
        &user.audit_actor(),
        &pet,
        name,
        clean_optional(form.active_ingredient.as_deref(), 120),
        form.concentration_value,
        clean_optional(form.concentration_unit.as_deref(), 40),
        form.dose_value,
        clean_optional(form.dose_unit.as_deref(), 30),
        clean_optional(form.frequency.as_deref(), 80),
        clean_optional(form.route.as_deref(), 40),
        clean_optional(form.instructions.as_deref(), 500),
        clean_optional(form.started_on.as_deref(), 20),
        status,
        clean_optional(form.raw_input.as_deref(), 1000),
    )
    .await?;
    Ok(Redirect::to(&format!("/app?pet={}", pet.id)))
}

#[derive(Deserialize)]
struct AdherenceForm {
    pet_id: i64,
    prescription_id: i64,
    scheduled_for: String,
    actual_dose_value: Option<f64>,
    actual_dose_unit: Option<String>,
    status: String,
    reason: Option<String>,
    raw_input: Option<String>,
}

async fn create_adherence(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Form(form): Form<AdherenceForm>,
) -> Result<Redirect, AppError> {
    let pet = db::get_pet(&state.db, user.household_id, form.pet_id)
        .await?
        .ok_or_else(AppError::not_found)?;
    let prescription =
        db::get_prescription(&state.db, user.household_id, pet.id, form.prescription_id)
            .await?
            .ok_or_else(AppError::not_found)?;
    let scheduled_for = clean_required(&form.scheduled_for, 20, "Date")?;
    if chrono::NaiveDate::parse_from_str(scheduled_for, "%Y-%m-%d").is_err() {
        return Err(AppError::validation("Use a valid date."));
    }
    let status = clean_required(&form.status, 30, "Status")?;
    if !matches!(status, "given" | "partial" | "missed" | "unknown") {
        return Err(AppError::validation("Choose a supported adherence status."));
    }
    db::create_medication_adherence(
        &state.db,
        user.household_id,
        &user.audit_actor(),
        &pet,
        &prescription,
        scheduled_for,
        form.actual_dose_value,
        clean_optional(form.actual_dose_unit.as_deref(), 30),
        status,
        clean_optional(form.reason.as_deref(), 500),
        clean_optional(form.raw_input.as_deref(), 1000),
    )
    .await?;
    Ok(Redirect::to(&format!("/app?pet={}", pet.id)))
}

async fn import_blood_tests(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
) -> Result<Redirect, AppError> {
    let pets = db::list_pets(&state.db, user.household_id).await?;
    let imported = ocr::import_directory(
        &state.config,
        &state.db,
        user.household_id,
        &user.audit_actor(),
        &pets,
    )
    .await?;
    let imported_count = imported
        .iter()
        .filter(|item| item.report_id.is_some())
        .count();
    tracing::info!(user = user.id, imported_count, "blood-test import finished");
    Ok(Redirect::to("/app"))
}

async fn upload_blood_test(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Response, AppError> {
    let mut uploaded = false;
    // `pet_id` is only ever present when this form was rendered by
    // `record_form_lab` (the Labs tab's standalone upload form predates it and
    // has no such field) — it says which pet's Labs tab to refresh below. The
    // upload itself stays household-wide, matching `ocr::import_directory`'s
    // existing behaviour, so a missing or invalid `pet_id` here does not
    // affect whether the upload succeeds, only which OOB view (if any) comes
    // back with it.
    let mut pet_id: Option<i64> = None;
    while let Some(field) = multipart.next_field().await? {
        match field.name() {
            Some("file") => {
                let filename = field
                    .file_name()
                    .ok_or_else(|| AppError::validation("Choose a blood-test file."))?
                    .to_owned();
                let bytes = field.bytes().await?;
                ocr::store_upload(&state.config, user.household_id, &filename, &bytes).await?;
                uploaded = true;
            }
            Some("pet_id") => {
                pet_id = field
                    .text()
                    .await
                    .ok()
                    .and_then(|text| text.trim().parse().ok());
            }
            _ => {}
        }
    }
    if !uploaded {
        return Err(AppError::validation("Choose a blood-test file."));
    }
    let pets = db::list_pets(&state.db, user.household_id).await?;
    let imported = ocr::import_directory(
        &state.config,
        &state.db,
        user.household_id,
        &user.audit_actor(),
        &pets,
    )
    .await?;
    let imported_count = imported
        .iter()
        .filter(|item| item.report_id.is_some())
        .count();
    tracing::info!(user = user.id, imported_count, "blood-test upload finished");

    // Household-scoped exactly like every other fragment route (`AGENTS.md`):
    // `pet_id` (a hidden form field, not directly attacker-controlled, but
    // never trusted regardless) only selects which pet's Labs tab is shown
    // back after re-checking it via `db::get_pet`.
    let scoped_pet = match pet_id {
        Some(id) => db::get_pet(&state.db, user.household_id, id).await?,
        None => None,
    };
    if wants_fragment(&headers) {
        return match scoped_pet {
            Some(pet) => {
                let lab_reports =
                    db::list_lab_reports(&state.db, user.household_id, pet.id).await?;
                let rendered = TabLabsTemplate { pet, lab_reports }.render()?;
                // See `as_refresh_template`'s doc comment: this is a manual
                // client-side swap, not htmx's `hx-swap-oob`, so a "+ Record
                // → Lab" upload from a tab other than Labs doesn't log
                // `htmx:oobErrorNoTarget`.
                let html = as_refresh_template("labs-refresh", rendered);
                Ok((StatusCode::OK, Html(html)).into_response())
            }
            // No usable pet context: still a successful upload, just nothing
            // to refresh in place — the dialog still closes on success.
            None => Ok(Html(String::new()).into_response()),
        };
    }
    let redirect = match scoped_pet {
        Some(pet) => format!("/app?pet={}&view=labs", pet.id),
        None => "/app".to_owned(),
    };
    Ok(Redirect::to(&redirect).into_response())
}

#[derive(Deserialize)]
struct CaptureForm {
    message: String,
    selected_pet_id: Option<i64>,
}

#[derive(Deserialize)]
struct ChatForm {
    message: String,
    pet_id: i64,
    view: Option<String>,
    mode: Option<String>,
    history: Option<String>,
}

#[derive(Deserialize)]
struct ConfirmMedicationPlanForm {
    pet_id: i64,
    view: Option<String>,
    history: Option<String>,
    confirmation_token: String,
    medication_name: String,
    dose_value: f64,
    dose_unit: String,
    frequency: String,
    reason: Option<String>,
    raw_input: String,
}

#[derive(Clone, Debug)]
struct PendingMedicationChange {
    change: MedicationPlanChange,
    raw_input: String,
    confirmation_token: String,
    replaces_existing: bool,
}

async fn capture(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    headers: HeaderMap,
    Form(form): Form<CaptureForm>,
) -> Result<Response, AppError> {
    record_agent_event(
        &state,
        &user,
        &headers,
        &form.message,
        form.selected_pet_id,
        ConsoleView::Timeline,
        None,
    )
    .await
}

async fn agent_chat(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    headers: HeaderMap,
    Form(form): Form<ChatForm>,
) -> Result<Response, AppError> {
    let view = ConsoleView::parse(form.view.as_deref());
    if form.mode.as_deref() == Some("record") {
        return record_agent_event(
            &state,
            &user,
            &headers,
            &form.message,
            Some(form.pet_id),
            view,
            form.history.as_deref(),
        )
        .await;
    }
    answer_agent_chat(&state, &user, &headers, form, view).await
}

async fn confirm_agent_medication_plan(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    headers: HeaderMap,
    Form(form): Form<ConfirmMedicationPlanForm>,
) -> Result<Response, AppError> {
    let view = ConsoleView::parse(form.view.as_deref());
    let pet = db::get_pet(&state.db, user.household_id, form.pet_id)
        .await?
        .ok_or_else(AppError::not_found)?;
    let medication_name = clean_required(&form.medication_name, 120, "Medication")?;
    if !form.dose_value.is_finite() || form.dose_value <= 0.0 || form.dose_value > 100_000.0 {
        return Err(AppError::validation(
            "Enter a valid dose greater than zero.",
        ));
    }
    let dose_unit = clean_required(&form.dose_unit, 30, "Dose unit")?;
    let frequency = clean_required(&form.frequency, 80, "Frequency")?;
    let reason = clean_optional(form.reason.as_deref(), 500);
    let raw_input = clean_required(&form.raw_input, 1000, "Original wording")?;
    let confirmation_token = clean_required(&form.confirmation_token, 128, "Confirmation")?;
    if confirmation_token.len() < 32
        || !confirmation_token
            .bytes()
            .all(|value| value.is_ascii_alphanumeric())
    {
        return Err(AppError::validation(
            "That confirmation has expired. Try the note again.",
        ));
    }

    let result = db::confirm_medication_plan_change(
        &state.db,
        user.household_id,
        &user.audit_actor(),
        &pet,
        &auth::token_hash(confirmation_token),
        medication_name,
        form.dose_value,
        dose_unit,
        frequency,
        reason,
        raw_input,
    )
    .await?;

    let summary = format!(
        "{medication_name}: {} {dose_unit} {frequency}",
        form.dose_value
    );
    let answer = if result.already_applied {
        format!("Already confirmed: {summary}. No duplicate plan was created.")
    } else if result.replaced_prescriptions > 0 {
        format!(
            "Confirmed: {summary}. The previous active {medication_name} plan was marked stopped."
        )
    } else {
        format!(
            "Confirmed: {summary} is now active in {}’s medication plan.",
            pet.name
        )
    };
    let mut evidence = vec![
        AssistantEvidence {
            label: "Human approval".into(),
            detail: "confirmed before changing the plan".into(),
            href: Some(format!("/app?pet={}&view=plan", pet.id)),
        },
        AssistantEvidence {
            label: "Original wording".into(),
            detail: "saved with the plan and timeline event".into(),
            href: Some(format!("/app?pet={}&view=timeline", pet.id)),
        },
    ];
    if let Some(reason) = reason {
        evidence.push(AssistantEvidence {
            label: "Reason/context".into(),
            detail: reason.to_owned(),
            href: None,
        });
    }
    let reply = AssistantReply {
        kind: "answer".into(),
        title: "MEDICATION PLAN CONFIRMED".into(),
        answer: answer.clone(),
        evidence,
        suggested_prompts: vec![
            "Show the current medication plan".into(),
            "Prepare questions for our next vet visit".into(),
        ],
    };
    let display_turns = parse_assistant_history(form.history.as_deref());
    let mut stored_history = display_turns.clone();
    stored_history.push(AssistantTurn {
        role: "assistant".into(),
        content: answer,
    });
    let assistant_html = render_assistant_workbench_with_history(
        &pet,
        view,
        "record",
        display_turns,
        stored_history,
        Some(reply),
        None,
    )?;
    let timeline = render_agent_timeline(&state, user.household_id, Some(pet.clone())).await?;
    let events_count = db::list_events(&state.db, user.household_id, Some(pet.id), 50)
        .await?
        .len();
    let data = load_console_data(&state, user.household_id, pet.id).await?;
    let active_prescription_count = data
        .prescriptions
        .iter()
        .filter(|item| item.status == "active")
        .count();
    let tab_refresh = if view.is_plan() {
        let tab_html = render_tab_from_data(&state, user.household_id, &pet, view, data).await?;
        Some(as_refresh_template(
            "tab-refresh",
            format!(r#"<div id="tab-body" class="tab-body">{tab_html}</div>"#),
        ))
    } else {
        None
    };
    let mut extra_html = ActivePrescriptionsMetricTemplate {
        pet: pet.clone(),
        active_prescription_count,
    }
    .render()
    .map(|html| as_refresh_template("active-prescriptions-refresh", html))?;
    if let Some(tab_refresh) = tab_refresh {
        extra_html.push_str(&tab_refresh);
    }
    assistant_fragment_response(
        &headers,
        assistant_html,
        Some(timeline.render()?),
        Some(events_count),
        Some(extra_html),
    )
}

async fn record_agent_event(
    state: &AppState,
    user: &UserAccount,
    headers: &HeaderMap,
    raw_message: &str,
    selected_pet_id: Option<i64>,
    view: ConsoleView,
    raw_history: Option<&str>,
) -> Result<Response, AppError> {
    let message = clean_required(raw_message, 1000, "Observation")?.to_owned();
    let pets = db::list_pets(&state.db, user.household_id).await?;
    let names: Vec<_> = pets.iter().map(|pet| pet.name.clone()).collect();
    let selected_pet = selected_from(state, user.household_id, &pets, selected_pet_id).await?;
    let selected_pet_name = selected_pet.as_ref().map(|pet| pet.name.as_str());
    let intent = match state
        .agent
        .propose_capture(&message, &names, selected_pet_name)
        .await
    {
        Ok(value) => value,
        Err(error) => {
            return assistant_error_response(
                headers,
                selected_pet.as_ref(),
                view,
                raw_history,
                "RECORD NEEDS CLARIFICATION",
                error.to_string(),
                StatusCode::UNPROCESSABLE_ENTITY,
            );
        }
    };
    let pet = db::find_pet_by_name(&state.db, user.household_id, &intent.event.pet_name)
        .await?
        .ok_or_else(|| AppError::validation("That pet no longer exists."))?;
    if let Some(change) = intent.medication_plan_change {
        let prescriptions =
            db::list_prescriptions(&state.db, user.household_id, pet.id, 100).await?;
        let replaces_existing = prescriptions.iter().any(|prescription| {
            prescription.status == "active"
                && prescription
                    .name
                    .eq_ignore_ascii_case(&change.medication_name)
        });
        let mut history = parse_assistant_history(raw_history);
        history.push(AssistantTurn {
            role: "user".into(),
            content: message.clone(),
        });
        let pending = PendingMedicationChange {
            change,
            raw_input: message,
            confirmation_token: auth::new_action_token(),
            replaces_existing,
        };
        let assistant_html = render_assistant_workbench_with_history(
            &pet,
            view,
            "record",
            history.clone(),
            history,
            None,
            Some(pending),
        )?;
        return assistant_fragment_response(&headers, assistant_html, None, None, None);
    }
    let received_at = Utc::now();
    let occurred_at = state.agent.occurred_at(&intent.event, received_at);
    db::create_health_event(
        &state.db,
        user.household_id,
        &user.audit_actor(),
        &pet,
        &intent.event,
        &message,
        occurred_at,
        "owner_agent",
    )
    .await?;
    let mut missed_prescriptions = 0;
    if intent.missed_medication {
        let scheduled_for = occurred_at.date_naive().to_string();
        let prescriptions =
            db::list_prescriptions(&state.db, user.household_id, pet.id, 100).await?;
        for prescription in prescriptions.iter().filter(|item| item.status == "active") {
            db::create_medication_adherence(
                &state.db,
                user.household_id,
                &user.audit_actor(),
                &pet,
                prescription,
                &scheduled_for,
                None,
                None,
                "missed",
                Some("No medication was given."),
                Some(&message),
            )
            .await?;
            missed_prescriptions += 1;
        }
    }
    let capture_message = if missed_prescriptions > 0 {
        format!(
            "Saved: {} Recorded missed doses for {} active prescription(s).",
            intent.event.summary, missed_prescriptions
        )
    } else {
        format!("Saved: {}", intent.event.summary)
    };
    let capture_title = if intent.used_model {
        "AI STRUCTURED THE EVENT"
    } else {
        "RECORDED IN THE TIMELINE"
    };
    let mut history = parse_assistant_history(raw_history);
    let mut display_turns = history.clone();
    display_turns.push(AssistantTurn {
        role: "user".into(),
        content: message.clone(),
    });
    history.push(AssistantTurn {
        role: "user".into(),
        content: message,
    });
    history.push(AssistantTurn {
        role: "assistant".into(),
        content: capture_message.clone(),
    });
    let mut evidence = vec![AssistantEvidence {
        label: "Original wording".into(),
        detail: "saved with the event".into(),
        href: Some(format!("/app?pet={}&view=timeline", pet.id)),
    }];
    if intent.used_model {
        evidence.push(AssistantEvidence {
            label: "AI classification".into(),
            detail: format!("{} · {}", intent.event.event_type, intent.event.concept),
            href: None,
        });
    }
    let reply = AssistantReply {
        kind: "answer".into(),
        title: capture_title.into(),
        answer: capture_message,
        evidence,
        suggested_prompts: vec![
            "Summarize the recent history".into(),
            "Prepare questions for our next vet visit".into(),
        ],
    };
    let assistant_html = render_assistant_workbench_with_history(
        &pet,
        view,
        "record",
        display_turns,
        history,
        Some(reply),
        None,
    )?;
    let timeline = render_agent_timeline(state, user.household_id, Some(pet.clone())).await?;
    let events_count = db::list_events(&state.db, user.household_id, Some(pet.id), 50)
        .await?
        .len();
    assistant_fragment_response(
        headers,
        assistant_html,
        Some(timeline.render()?),
        Some(events_count),
        None,
    )
}

async fn answer_agent_chat(
    state: &AppState,
    user: &UserAccount,
    headers: &HeaderMap,
    form: ChatForm,
    view: ConsoleView,
) -> Result<Response, AppError> {
    let question = clean_required(&form.message, 1000, "Question")?.to_owned();
    let pet = db::get_pet(&state.db, user.household_id, form.pet_id)
        .await?
        .ok_or_else(AppError::not_found)?;
    let entries = db::list_timeline(&state.db, user.household_id, pet.id, None, 50).await?;
    let (context, evidence) = chat_context(&pet, &entries);
    let history = parse_assistant_history(form.history.as_deref());
    let history_text = history
        .iter()
        .map(|turn| format!("{}: {}", turn.role, turn.content))
        .collect::<Vec<_>>()
        .join("\n");
    let reply = match state
        .chat
        .answer(
            &question,
            &pet.name,
            &pet.species,
            view.label(),
            &context,
            &history_text,
        )
        .await
    {
        Ok(Some(reply)) => assistant_reply_from_chat(reply, evidence),
        Ok(None) => fallback_chat_reply(&question, &pet, &entries, evidence),
        Err(error) => AssistantReply {
            kind: "clarification".into(),
            title: "COPILOT UNAVAILABLE".into(),
            answer: format!(
                "{error} Your record is unchanged. You can still use Record mode or try again."
            ),
            evidence: Vec::new(),
            suggested_prompts: vec!["Record what happened".into()],
        },
    };
    let mut display_turns = history.clone();
    display_turns.push(AssistantTurn {
        role: "user".into(),
        content: question,
    });
    let mut stored_history = display_turns.clone();
    stored_history.push(AssistantTurn {
        role: "assistant".into(),
        content: reply.answer.clone(),
    });
    let assistant_html = render_assistant_workbench_with_history(
        &pet,
        view,
        "ask",
        display_turns,
        stored_history,
        Some(reply),
        None,
    )?;
    assistant_fragment_response(headers, assistant_html, None, None, None)
}

fn assistant_error_response(
    headers: &HeaderMap,
    pet: Option<&Pet>,
    view: ConsoleView,
    raw_history: Option<&str>,
    title: &str,
    answer: String,
    status: StatusCode,
) -> Result<Response, AppError> {
    let Some(pet) = pet else {
        return Ok((status, Html(escape(&answer))).into_response());
    };
    let history = parse_assistant_history(raw_history);
    let reply = AssistantReply {
        kind: "clarification".into(),
        title: title.into(),
        answer,
        evidence: Vec::new(),
        suggested_prompts: vec!["Summarize the recent history".into()],
    };
    let html = render_assistant_workbench_with_history(
        pet,
        view,
        "record",
        history.clone(),
        history,
        Some(reply),
        None,
    )?;
    let response = as_refresh_template("assistant-refresh", html);
    if !wants_fragment(headers) {
        return Ok((status, Html(response)).into_response());
    }
    Ok((status, Html(response)).into_response())
}

fn assistant_fragment_response(
    headers: &HeaderMap,
    assistant_html: String,
    timeline_html: Option<String>,
    events_count: Option<usize>,
    extra_html: Option<String>,
) -> Result<Response, AppError> {
    if !wants_fragment(headers) {
        return Ok(Redirect::to("/app").into_response());
    }
    let mut html = as_refresh_template("assistant-refresh", assistant_html);
    if let Some(timeline_html) = timeline_html {
        html.push_str(&as_refresh_template("timeline-refresh", timeline_html));
    }
    if let Some(events_count) = events_count {
        html.push_str(&as_refresh_template(
            "events-refresh",
            EventsMetricTemplate { events_count }.render()?,
        ));
    }
    if let Some(extra_html) = extra_html {
        html.push_str(&extra_html);
    }
    Ok((StatusCode::OK, Html(html)).into_response())
}

fn parse_assistant_history(raw: Option<&str>) -> Vec<AssistantTurn> {
    raw.and_then(|value| serde_json::from_str::<Vec<AssistantTurn>>(value).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|turn| matches!(turn.role.as_str(), "user" | "assistant"))
        .map(|mut turn| {
            turn.content = turn.content.chars().take(1000).collect();
            turn
        })
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn assistant_reply_from_chat(reply: ChatReply, evidence: Vec<AssistantEvidence>) -> AssistantReply {
    let (kind, title) = match reply.kind.as_str() {
        "clarification" => ("clarification", "A LITTLE MORE CONTEXT"),
        "safety" => ("safety", "SAFETY BOUNDARY"),
        _ => ("answer", "RECORD ANALYST"),
    };
    AssistantReply {
        kind: kind.into(),
        title: title.into(),
        answer: reply.answer,
        evidence,
        suggested_prompts: reply.suggested_prompts,
    }
}

fn chat_context(pet: &Pet, entries: &[TimelineEntry]) -> (String, Vec<AssistantEvidence>) {
    let href = format!("/app?pet={}&view=timeline", pet.id);
    let mut lines = vec![format!("Pet: {} ({})", pet.name, pet.species)];
    let mut evidence = Vec::new();
    for entry in entries.iter().take(12) {
        match entry {
            TimelineEntry::Event(event) => {
                lines.push(format!(
                    "{} | observation | {} | original wording: {}",
                    event.occurred_at.to_rfc3339(),
                    event.summary,
                    event.raw_input
                ));
                evidence.push(AssistantEvidence {
                    label: event.summary.clone(),
                    detail: event.occurred_at.format("%d %b").to_string(),
                    href: Some(href.clone()),
                });
            }
            TimelineEntry::Weight(weight) => {
                lines.push(format!(
                    "{} | weight | {:.2} kg{}",
                    weight.measured_at.to_rfc3339(),
                    weight.weight_kg,
                    weight
                        .note
                        .as_deref()
                        .map(|note| format!(" | note: {note}"))
                        .unwrap_or_default()
                ));
                evidence.push(AssistantEvidence {
                    label: format!("{:.2} kg", weight.weight_kg),
                    detail: weight.measured_at.format("%d %b").to_string(),
                    href: Some(href.clone()),
                });
            }
            TimelineEntry::Dose(dose) => {
                lines.push(format!(
                    "{} | medication | {} | status: {}",
                    dose.administered_at.to_rfc3339(),
                    dose.name,
                    dose.status
                ));
                evidence.push(AssistantEvidence {
                    label: dose.name.clone(),
                    detail: dose.administered_at.format("%d %b").to_string(),
                    href: Some(href.clone()),
                });
            }
            TimelineEntry::Lab(lab) => {
                let results = lab
                    .results
                    .iter()
                    .take(6)
                    .map(|result| format!("{}={}", result.test_name, result.value_text))
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(format!(
                    "{} | lab report | {} | results: {}",
                    lab.test_date.as_deref().unwrap_or("date unavailable"),
                    lab.source_filename,
                    results
                ));
                evidence.push(AssistantEvidence {
                    label: "Lab report".into(),
                    detail: lab
                        .test_date
                        .clone()
                        .unwrap_or_else(|| "import date".into()),
                    href: Some(format!("/app?pet={}&view=labs", pet.id)),
                });
            }
        }
    }
    (lines.join("\n"), evidence.into_iter().take(6).collect())
}

fn fallback_chat_reply(
    question: &str,
    pet: &Pet,
    entries: &[TimelineEntry],
    evidence: Vec<AssistantEvidence>,
) -> AssistantReply {
    let lower = question.to_lowercase();
    if lower.contains("diagnos") || lower.contains("what is wrong") {
        return AssistantReply {
            kind: "safety".into(),
            title: "SAFETY BOUNDARY".into(),
            answer: "I can summarize recorded facts and help prepare questions, but I cannot diagnose your pet. A veterinarian should interpret symptoms, especially if they are severe, worsening, or accompanied by urgent signs.".into(),
            evidence,
            suggested_prompts: vec![
                "Summarize the recent history".into(),
                "Prepare questions for our next vet visit".into(),
            ],
        };
    }
    if lower.contains("weight") || lower.contains("weigh") {
        let weights = entries
            .iter()
            .filter_map(|entry| match entry {
                TimelineEntry::Weight(weight) => Some(weight),
                _ => None,
            })
            .collect::<Vec<_>>();
        let answer = match weights.first() {
            Some(latest) => {
                let previous = weights
                    .get(1)
                    .map(|weight| latest.weight_kg - weight.weight_kg);
                match previous {
                    Some(delta) => format!(
                        "The latest recorded weight for {} is {:.2} kg on {}. That is {:+.2} kg compared with the previous recorded measurement.",
                        pet.name,
                        latest.weight_kg,
                        latest.measured_at.format("%d %b %Y"),
                        delta
                    ),
                    None => format!(
                        "The latest recorded weight for {} is {:.2} kg on {}. There is only one dated measurement in the record.",
                        pet.name,
                        latest.weight_kg,
                        latest.measured_at.format("%d %b %Y")
                    ),
                }
            }
            None => format!(
                "There are no dated weight measurements recorded for {} yet.",
                pet.name
            ),
        };
        return AssistantReply {
            kind: "answer".into(),
            title: "WEIGHT CHECK".into(),
            answer,
            evidence,
            suggested_prompts: vec!["Summarize the recent history".into()],
        };
    }
    let recent = entries
        .iter()
        .take(4)
        .map(|entry| match entry {
            TimelineEntry::Event(event) => {
                format!("{} ({})", event.summary, event.occurred_at.format("%d %b"))
            }
            TimelineEntry::Weight(weight) => format!(
                "Weight {:.2} kg ({})",
                weight.weight_kg,
                weight.measured_at.format("%d %b")
            ),
            TimelineEntry::Dose(dose) => format!(
                "{} recorded ({})",
                dose.name,
                dose.administered_at.format("%d %b")
            ),
            TimelineEntry::Lab(lab) => format!(
                "Lab report ({})",
                lab.test_date.as_deref().unwrap_or("date unavailable")
            ),
        })
        .collect::<Vec<_>>();
    let answer = if recent.is_empty() {
        format!(
            "I do not have any dated timeline entries for {} yet. You can switch to Record mode and describe what happened.",
            pet.name
        )
    } else {
        format!(
            "The most recent recorded items for {} are: {}. These are record summaries, not a diagnosis.",
            pet.name,
            recent.join("; ")
        )
    };
    AssistantReply {
        kind: "answer".into(),
        title: "RECENT HISTORY".into(),
        answer,
        evidence,
        suggested_prompts: vec![
            "What has changed in the last 7 days?".into(),
            "Prepare questions for our next vet visit".into(),
        ],
    }
}

async fn undo_event(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Path(id): Path<i64>,
    Query(query): Query<IndexQuery>,
) -> Result<Html<String>, AppError> {
    db::undo_event(&state.db, user.household_id, &user.audit_actor(), id).await?;
    let pets = db::list_pets(&state.db, user.household_id).await?;
    let selected_pet = selected_from(&state, user.household_id, &pets, query.pet).await?;
    let template = render_agent_timeline(&state, user.household_id, selected_pet.clone()).await?;
    let mut html = as_refresh_template("timeline-refresh", template.render()?);
    let events_count = match selected_pet {
        Some(pet) => db::list_events(&state.db, user.household_id, Some(pet.id), 50)
            .await?
            .len(),
        None => 0,
    };
    html.push_str(&as_refresh_template(
        "events-refresh",
        EventsMetricTemplate { events_count }.render()?,
    ));
    Ok(Html(html))
}

#[derive(Deserialize)]
struct EventSummaryForm {
    pet_id: i64,
    summary: String,
}

async fn update_event_summary(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Path(id): Path<i64>,
    Form(form): Form<EventSummaryForm>,
) -> Result<Html<String>, AppError> {
    let summary = clean_required(&form.summary, 120, "Label")?;
    let updated = db::update_event_summary(
        &state.db,
        user.household_id,
        form.pet_id,
        &user.audit_actor(),
        id,
        summary,
    )
    .await?;
    if !updated {
        return Err(AppError::not_found());
    }

    let pet = db::get_pet(&state.db, user.household_id, form.pet_id)
        .await?
        .ok_or_else(AppError::not_found)?;
    let timeline = render_agent_timeline(&state, user.household_id, Some(pet)).await?;
    Ok(Html(as_refresh_template(
        "timeline-refresh",
        timeline.render()?,
    )))
}

#[derive(Deserialize)]
struct TimelineOlderQuery {
    pet: i64,
    before_at: String,
    before_id: i64,
}

/// `GET /app/timeline/older` — the "Load older" affordance on the Timeline tab.
/// Household-scoped the same way every other fragment route is: the pet id from
/// the query string is resolved through `db::get_pet(household_id, pet_id)`
/// before it's trusted for anything. Returns the next page of timeline rows plus
/// an out-of-band replacement of the `#timeline-load-more` button (see
/// `_timeline_load_more.html`), so the client only needs `hx-swap="beforeend"`
/// on the visible rows and never has to manage the trigger itself.
async fn timeline_older(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Query(query): Query<TimelineOlderQuery>,
) -> Result<Html<String>, AppError> {
    let pet = db::get_pet(&state.db, user.household_id, query.pet)
        .await?
        .ok_or_else(AppError::not_found)?;
    let before_at = DateTime::parse_from_rfc3339(&query.before_at)
        .map(|at| at.with_timezone(&Utc))
        .map_err(|_| AppError::validation("Invalid pagination cursor."))?;
    let before = Some((before_at, query.before_id));
    let raw_entries = db::list_timeline(
        &state.db,
        user.household_id,
        pet.id,
        before,
        TIMELINE_PAGE_SIZE + 1,
    )
    .await?;
    let (entries, next_cursor) = paginate_timeline(raw_entries, TIMELINE_PAGE_SIZE);
    let days = group_timeline_by_day(entries, Some(before_at.date_naive()));
    render(&TimelineOlderTemplate {
        pet_id: pet.id,
        days,
        next_cursor,
        load_more_oob: true,
    })
}

#[derive(Deserialize)]
struct ShareForm {
    pet_id: i64,
    label: String,
    days: i64,
}

async fn create_share(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Form(form): Form<ShareForm>,
) -> Result<Html<String>, AppError> {
    if db::get_pet(&state.db, user.household_id, form.pet_id)
        .await?
        .is_none()
    {
        return Err(AppError::not_found());
    }
    let label = clean_required(&form.label, 120, "Vet or clinic")?;
    let created = db::create_share(
        &state.db,
        user.household_id,
        &user.audit_actor(),
        form.pet_id,
        label,
        form.days,
    )
    .await?;
    let shares = db::list_shares(&state.db, user.household_id).await?;
    render(&SharesTemplate {
        shares,
        new_share_path: created.token.map(|token| format!("/share/{token}")),
    })
}

async fn revoke_share(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Path(id): Path<i64>,
) -> Result<Html<String>, AppError> {
    db::revoke_share(&state.db, user.household_id, &user.audit_actor(), id).await?;
    let shares = db::list_shares(&state.db, user.household_id).await?;
    render(&SharesTemplate {
        shares,
        new_share_path: None,
    })
}

async fn shared_pet(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<Html<String>, AppError> {
    let (grant, pet) = db::resolve_share(&state.db, &token)
        .await?
        .ok_or_else(AppError::not_found)?;
    let events = db::list_events(&state.db, grant.household_id, Some(pet.id), 100).await?;
    render(&SharedPetTemplate { grant, pet, events })
}

async fn css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], CSS)
}

async fn js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("../static/app.js"),
    )
}
async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}
async fn favicon() -> impl IntoResponse {
    StatusCode::NO_CONTENT
}

async fn require_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let token = session_token(&state, request.headers()).map(str::to_owned);
    let user = match token {
        Some(token) => db::resolve_session(&state.db, &token).await.ok().flatten(),
        None => None,
    };
    if user.is_some() && !same_origin(request.method(), request.headers()) {
        tracing::warn!(
            host = ?request.headers().get(header::HOST),
            origin = ?request.headers().get(header::ORIGIN),
            "rejected cross-origin authenticated mutation"
        );
        (StatusCode::FORBIDDEN, "Cross-origin mutation rejected").into_response()
    } else if let Some(user) = user {
        let mut request = request;
        request.extensions_mut().insert(user);
        next.run(request).await
    } else if matches!(*request.method(), Method::GET | Method::HEAD) {
        Redirect::to("/login").into_response()
    } else {
        (StatusCode::UNAUTHORIZED, "Sign in required").into_response()
    }
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static("default-src 'self'; script-src 'self' https://cdn.jsdelivr.net; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"),
    );
    response
}

async fn selected_from(
    state: &AppState,
    household_id: i64,
    pets: &[Pet],
    requested: Option<i64>,
) -> Result<Option<Pet>, AppError> {
    match requested.or_else(|| pets.first().map(|pet| pet.id)) {
        Some(id) => Ok(db::get_pet(&state.db, household_id, id).await?),
        None => Ok(None),
    }
}

/// Builds the merged timeline fragment for the selected pet. Every caller has
/// already resolved the pet through a household-scoped lookup.
async fn render_agent_timeline(
    state: &AppState,
    household_id: i64,
    selected_pet: Option<Pet>,
) -> Result<AgentTimelineTemplate, AppError> {
    let (pet_id, days, entries_count, next_cursor) = match &selected_pet {
        Some(pet) => {
            let raw_entries = db::list_timeline(
                &state.db,
                household_id,
                pet.id,
                None,
                TIMELINE_PAGE_SIZE + 1,
            )
            .await?;
            let (entries, next_cursor) = paginate_timeline(raw_entries, TIMELINE_PAGE_SIZE);
            let entries_count = entries.len();
            (
                pet.id,
                group_timeline_by_day(entries, None),
                entries_count,
                next_cursor,
            )
        }
        // No pet in the household at all: nothing to show, and pet_id is never
        // read by the template in this branch (next_cursor is always None here).
        None => (0, Vec::new(), 0, None),
    };
    Ok(AgentTimelineTemplate {
        pet_id,
        days,
        entries_count,
        next_cursor,
        load_more_oob: false,
    })
}

/// Wraps an HTML fragment in an inert, unswapped `<template>` tagged with
/// `id`. `<template>` content lives in a separate document fragment — it is
/// never rendered and, critically, htmx's own out-of-band swap scan
/// (`querySelectorAll('[hx-swap-oob]')` over the *response* fragment) does
/// not descend into it, so nothing here is ever a candidate for htmx's own
/// oob handling. `static/app.js`'s `htmx:afterRequest` handler pulls this
/// back out of `event.detail.xhr.responseText` with a `DOMParser` and swaps
/// it in manually with plain `Element.replaceWith`, but *only* if the target
/// id is actually present in the current page — otherwise it's a no-op.
///
/// This exists because htmx's built-in `hx-swap-oob="true"` does the
/// opposite of what `UI_REDESIGN_PLAN.md` §4A needs here: when no element
/// with the given id exists in the requester's current DOM (e.g. the
/// "+ Record" dialog was opened from the Plan tab, so `#agent-and-timeline`
/// isn't on screen), htmx doesn't silently skip it — it logs
/// `htmx:oobErrorNoTarget` to the console, which is a real console error
/// hit on every non-Timeline-tab weight/dose/symptom save. Manual swapping
/// gets the "quietly do nothing if it isn't there" behaviour this route
/// actually wants.
fn as_refresh_template(id: &str, html: String) -> String {
    format!(r#"<template id="{id}">{html}</template>"#)
}

/// Shared response for the three `+ Record` dialog forms that affect the
/// Timeline — weight, dose, and the structured symptom record
/// (`UI_REDESIGN_PLAN.md` §4A). An htmx request gets `#agent-and-timeline`'s
/// refreshed content wrapped for the manual swap described on
/// `as_refresh_template`; a plain, non-htmx submission — the
/// progressive-enhancement path — gets a normal redirect back to the
/// Timeline tab, matching what these routes did before Phase 4 moved their
/// forms into dialogs.
async fn timeline_write_response(
    state: &AppState,
    headers: &HeaderMap,
    household_id: i64,
    pet: Pet,
    refresh_latest_weight: bool,
) -> Result<Response, AppError> {
    if wants_fragment(headers) {
        let pet_id = pet.id;
        let template = render_agent_timeline(state, household_id, Some(pet)).await?;
        let mut html = as_refresh_template("timeline-refresh", template.render()?);
        let events_count = db::list_events(&state.db, household_id, Some(pet_id), 50)
            .await?
            .len();
        html.push_str(&as_refresh_template(
            "events-refresh",
            EventsMetricTemplate { events_count }.render()?,
        ));
        if refresh_latest_weight {
            let latest_weight = db::list_weights(&state.db, household_id, pet_id)
                .await?
                .into_iter()
                .next();
            html.push_str(&as_refresh_template(
                "latest-weight-header-refresh",
                LatestWeightHeaderTemplate {
                    latest_weight: latest_weight.clone(),
                }
                .render()?,
            ));
            html.push_str(&as_refresh_template(
                "latest-weight-metric-refresh",
                LatestWeightMetricTemplate { latest_weight }.render()?,
            ));
        }
        Ok((StatusCode::OK, Html(html)).into_response())
    } else {
        Ok(Redirect::to(&format!("/app?pet={}&view=timeline", pet.id)).into_response())
    }
}

/// Pre-existing bug, surfaced by Phase 4: axum's `Form` extractor (via
/// `serde_html_form`) does *not* treat an empty string as `None` for an
/// `Option<f64>`/`Option<i64>` field — it tries to parse `""` as the number
/// and the whole request 422s with a raw "Failed to deserialize form body"
/// error that never reaches `AppError` (so nothing renders in the dialog's
/// status area at all). `dose_value` and `occurrence_count` are the two such
/// fields the "+ Record" dialogs post — before Phase 4 they sat in a rarely
/// opened `<details>`, so leaving them blank was rare enough this went
/// unnoticed. Used via `#[serde(default, deserialize_with =
/// "empty_str_as_none")]`.
fn empty_str_as_none<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match Option::<String>::deserialize(deserializer)? {
        None => Ok(None),
        Some(value) if value.trim().is_empty() => Ok(None),
        Some(value) => value
            .trim()
            .parse()
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

fn clean_required<'a>(value: &'a str, max: usize, label: &str) -> Result<&'a str, AppError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(AppError::validation(format!("{label} is required.")));
    }
    if value.chars().count() > max {
        return Err(AppError::validation(format!("{label} is too long.")));
    }
    Ok(value)
}
fn clean_optional(value: Option<&str>, max: usize) -> Option<&str> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty() && v.chars().count() <= max)
}

fn normalize_email(value: &str) -> Result<String, AppError> {
    let email = clean_required(value, 254, "Email")?.to_ascii_lowercase();
    let mut parts = email.split('@');
    let local = parts.next().unwrap_or_default();
    let domain = parts.next().unwrap_or_default();
    if local.is_empty()
        || domain.is_empty()
        || !domain.contains('.')
        || parts.next().is_some()
        || email.chars().any(char::is_whitespace)
    {
        return Err(AppError::validation("Enter a valid email address."));
    }
    Ok(email)
}

fn cookie_name(state: &AppState) -> &'static str {
    if state.config.production {
        "__Host-pethealth_session"
    } else {
        "pethealth_session"
    }
}

fn session_token<'a>(state: &AppState, headers: &'a HeaderMap) -> Option<&'a str> {
    let name = cookie_name(state);
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

fn session_cookie(state: &AppState, token: &str, clear: bool) -> HeaderValue {
    let secure = if state.config.production {
        "; Secure"
    } else {
        ""
    };
    let max_age = if clear {
        "Max-Age=0"
    } else {
        "Max-Age=2592000"
    };
    HeaderValue::from_str(&format!(
        "{}={token}; Path=/; {max_age}; HttpOnly; SameSite=Lax{secure}",
        cookie_name(state)
    ))
    .expect("session cookie contains only safe characters")
}

fn session_redirect(location: &str, cookie: HeaderValue) -> Response {
    let mut response = Redirect::to(location).into_response();
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    response
}

fn safe_login_next(next: Option<&str>) -> Option<&str> {
    next.filter(|value| {
        (value.starts_with("/oauth/authorize?") || value.starts_with("/oauth/device?"))
            && !value.contains('\n')
            && !value.contains('\r')
    })
}

fn request_origin(state: &AppState, headers: &HeaderMap) -> String {
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .unwrap_or(if state.config.production {
            "https"
        } else {
            "http"
        });
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost:3000");
    format!("{scheme}://{host}")
}

fn same_origin(method: &Method, headers: &HeaderMap) -> bool {
    if matches!(*method, Method::GET | Method::HEAD) {
        return true;
    }
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    let Some(origin) = headers.get(header::ORIGIN) else {
        // SameSite=Lax prevents cookies on cross-site form posts. Browsers may omit
        // Origin on ordinary same-origin forms, especially under no-referrer.
        return true;
    };
    if origin == "null" {
        return headers
            .get("sec-fetch-site")
            .is_some_and(|value| value == "same-origin");
    }
    let origin_authority = origin
        .to_str()
        .ok()
        .and_then(|value| value.parse::<http::Uri>().ok())
        .and_then(|uri| uri.authority().map(|value| value.as_str().to_owned()));
    host.zip(origin_authority.as_deref())
        .is_some_and(|(host, origin)| host == origin)
}

fn require_form_origin(state: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    if !state.config.production || same_origin(&Method::POST, headers) {
        Ok(())
    } else {
        Err(AppError::forbidden(
            "Cross-origin form submission rejected.",
        ))
    }
}

fn render<T: Template>(template: &T) -> Result<Html<String>, AppError> {
    Ok(Html(template.render()?))
}
fn render_status<T: Template>(template: &T, status: StatusCode) -> Result<Response, AppError> {
    Ok((status, Html(template.render()?)).into_response())
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomeTemplate {
    mcp_url: String,
    device_url: String,
}

#[derive(Template)]
#[template(path = "console.html")]
struct ConsoleTemplate {
    user: UserAccount,
    pets: Vec<Pet>,
    selected_pet: Option<Pet>,
    view: ConsoleView,
    /// The selected tab, pre-rendered from its own template struct (see
    /// `render_tab_from_data`) and injected here with `|safe`. Not an
    /// `{% include %}`: an include would inherit this struct's fields, forcing
    /// `ConsoleTemplate` to keep carrying every tab's data on every load.
    tab_html: String,
    assistant_html: String,
    events_count: usize,
    latest_weight: Option<WeightEntry>,
    shares_count: usize,
    active_prescription_count: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AssistantTurn {
    role: String,
    content: String,
}

#[derive(Clone, Debug)]
struct AssistantEvidence {
    label: String,
    detail: String,
    href: Option<String>,
}

#[derive(Clone, Debug)]
struct AssistantReply {
    kind: String,
    title: String,
    answer: String,
    evidence: Vec<AssistantEvidence>,
    suggested_prompts: Vec<String>,
}

#[derive(Template)]
#[template(path = "_assistant_workbench.html")]
struct AssistantWorkbenchTemplate {
    pet: Pet,
    view_key: String,
    view_label: String,
    mode: String,
    history_json: String,
    turns: Vec<AssistantTurn>,
    reply: Option<AssistantReply>,
    pending_medication_change: Option<PendingMedicationChange>,
}

#[derive(Template)]
#[template(path = "_events_metric.html")]
struct EventsMetricTemplate {
    events_count: usize,
}

#[derive(Template)]
#[template(path = "_active_prescriptions_metric.html")]
struct ActivePrescriptionsMetricTemplate {
    pet: Pet,
    active_prescription_count: usize,
}

fn render_assistant_workbench(
    pet: &Pet,
    view: ConsoleView,
    mode: &str,
    turns: Vec<AssistantTurn>,
    reply: Option<AssistantReply>,
    pending_medication_change: Option<PendingMedicationChange>,
) -> Result<String, AppError> {
    let history_json = serde_json::to_string(&turns)?;
    Ok(AssistantWorkbenchTemplate {
        pet: pet.clone(),
        view_key: view.key().to_owned(),
        view_label: view.label().to_owned(),
        mode: mode.to_owned(),
        history_json,
        turns,
        reply,
        pending_medication_change,
    }
    .render()?)
}

fn render_assistant_workbench_with_history(
    pet: &Pet,
    view: ConsoleView,
    mode: &str,
    display_turns: Vec<AssistantTurn>,
    history_turns: Vec<AssistantTurn>,
    reply: Option<AssistantReply>,
    pending_medication_change: Option<PendingMedicationChange>,
) -> Result<String, AppError> {
    let history_json = serde_json::to_string(&history_turns)?;
    Ok(AssistantWorkbenchTemplate {
        pet: pet.clone(),
        view_key: view.key().to_owned(),
        view_label: view.label().to_owned(),
        mode: mode.to_owned(),
        history_json,
        turns: display_turns,
        reply,
        pending_medication_change,
    }
    .render()?)
}

#[derive(Template)]
#[template(path = "_latest_weight_header.html")]
struct LatestWeightHeaderTemplate {
    latest_weight: Option<WeightEntry>,
}

#[derive(Template)]
#[template(path = "_latest_weight_metric.html")]
struct LatestWeightMetricTemplate {
    latest_weight: Option<WeightEntry>,
}

#[derive(Template)]
#[template(path = "_tab_timeline.html")]
struct TabTimelineTemplate {
    pet_id: i64,
    days: Vec<TimelineDay>,
    entries_count: usize,
    next_cursor: Option<TimelineCursor>,
    /// `_tab_timeline.html` is just `{% include "_agent_timeline.html") %}`, so
    /// this struct must carry every field that template reads. See
    /// `_timeline_load_more.html`'s doc comment. Always `false` here.
    load_more_oob: bool,
}

#[derive(Template)]
#[template(path = "_tab_plan.html")]
struct TabPlanTemplate {
    pet: Pet,
    prescriptions: Vec<MedicationPrescription>,
    active_prescriptions: Vec<MedicationPrescription>,
    adherence: Vec<MedicationAdherence>,
    medications: Vec<MedicationAdministration>,
    weights: Vec<WeightEntry>,
}

#[derive(Template)]
#[template(path = "_tab_labs.html")]
struct TabLabsTemplate {
    pet: Pet,
    lab_reports: Vec<LabReport>,
}

#[derive(Template)]
#[template(path = "_tab_sharing.html")]
struct TabSharingTemplate {
    pet: Pet,
    shares: Vec<ShareGrant>,
    new_share_path: Option<String>,
}

/// Wraps a `+ Record` form partial (or the drawer body) for direct, non-htmx
/// navigation. See `standalone_or_fragment`.
#[derive(Template)]
#[template(path = "_standalone.html")]
struct StandaloneTemplate {
    title: String,
    pet_id: i64,
    pet_name: String,
    body: String,
}

#[derive(Template)]
#[template(path = "_form_weight.html")]
struct FormWeightTemplate {
    pet: Pet,
}

#[derive(Template)]
#[template(path = "_form_dose.html")]
struct FormDoseTemplate {
    pet: Pet,
}

#[derive(Template)]
#[template(path = "_form_symptom.html")]
struct FormSymptomTemplate {
    pet: Pet,
}

#[derive(Template)]
#[template(path = "_form_lab.html")]
struct FormLabTemplate {
    pet: Pet,
}

/// The transient knowledge drawer body (`UI_REDESIGN_PLAN.md` §4B): the
/// knowledge article for the clicked event's concept, plus how many other
/// active events share that concept.
#[derive(Template)]
#[template(path = "_drawer.html")]
struct DrawerTemplate {
    event: HealthEvent,
    knowledge: Option<KnowledgeArticle>,
    related_count: u64,
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    identifier: String,
    error: Option<String>,
    notice: Option<String>,
    next: Option<String>,
}

#[derive(Template)]
#[template(path = "register.html")]
struct RegisterTemplate {
    display_name: String,
    email: String,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "account.html")]
struct AccountTemplate {
    user: UserAccount,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "_agent_timeline.html")]
struct AgentTimelineTemplate {
    pet_id: i64,
    days: Vec<TimelineDay>,
    entries_count: usize,
    next_cursor: Option<TimelineCursor>,
    /// See `_timeline_load_more.html`'s doc comment. Always `false` here —
    /// `render_agent_timeline` is the only place that constructs this
    /// template, and every one of its callers either swaps
    /// `#agent-and-timeline` in directly or wraps this whole render for a
    /// manual swap (`as_refresh_template`), so this div should just ride
    /// along as ordinary content rather than be independently oob-swapped.
    load_more_oob: bool,
}

/// The `hx-get="/app/timeline/older"` response: the next page's rows plus an
/// out-of-band replacement of the "Load older" trigger. See `_timeline_older.html`.
#[derive(Template)]
#[template(path = "_timeline_older.html")]
struct TimelineOlderTemplate {
    pet_id: i64,
    days: Vec<TimelineDay>,
    next_cursor: Option<TimelineCursor>,
    /// See `_timeline_load_more.html`'s doc comment. Always `true` here: this
    /// is the one response where the "Load older" trigger lives outside the
    /// element (`#timeline-entries`) actually being swapped, so it needs its
    /// own independent out-of-band update.
    load_more_oob: bool,
}

#[derive(Template)]
#[template(path = "_shares.html")]
struct SharesTemplate {
    shares: Vec<ShareGrant>,
    new_share_path: Option<String>,
}

#[derive(Template)]
#[template(path = "shared_pet.html")]
struct SharedPetTemplate {
    grant: ShareGrant,
    pet: Pet,
    events: Vec<HealthEvent>,
}

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    message: String,
}
impl AppError {
    fn validation(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: message.into(),
        }
    }
    fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: "Not found".into(),
        }
    }
    fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
        }
    }
}
impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(error: E) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.into().to_string(),
        }
    }
}
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            self.status,
            Html(format!(
                "<section class=\"inline-error\" role=\"alert\">{}</section>",
                escape(&self.message)
            )),
        )
            .into_response()
    }
}
fn escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csrf_origin_accepts_browser_null_only_for_same_origin_fetches() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("pets.example:3000"));
        headers.insert(header::ORIGIN, HeaderValue::from_static("null"));
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        assert!(same_origin(&Method::POST, &headers));

        headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!same_origin(&Method::POST, &headers));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://attacker.example"),
        );
        assert!(!same_origin(&Method::POST, &headers));
    }
}
