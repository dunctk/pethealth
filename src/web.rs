use crate::{
    AppState, auth, db,
    domain::{HealthEvent, KnowledgeArticle, LabReport, Pet, ShareGrant, UserAccount, WeightEntry},
    ocr,
};
use askama::Template;
use axum::{
    Extension, Form, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use chrono::Utc;
use serde::Deserialize;

const CSS: &str = include_str!("../static/app.css");

pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/", get(index))
        .route("/pets", post(create_pet))
        .route("/weights", post(create_weight))
        .route("/blood-tests/import", post(import_blood_tests))
        .route("/agent/capture", post(capture))
        .route("/events/{id}/undo", post(undo_event))
        .route("/shares", post(create_share))
        .route("/shares/{id}/revoke", post(revoke_share))
        .route("/account", get(account_page))
        .route("/account/password", post(change_password))
        .route("/logout", post(logout))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));
    Router::new()
        .route("/healthz", get(healthz))
        .route("/favicon.ico", get(favicon))
        .route("/static/app.css", get(css))
        .route("/login", get(login_page).post(login))
        .route("/register", get(register_page).post(register))
        .route("/share/{token}", get(shared_pet))
        .route("/mcp", post(crate::mcp::endpoint))
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
    let destination = safe_login_next(form.next.as_deref()).unwrap_or("/");
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
    Ok(session_redirect("/", session_cookie(&state, &token, false)))
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
    let events = db::list_events(
        &state.db,
        user.household_id,
        selected_pet.as_ref().map(|pet| pet.id),
        50,
    )
    .await?;
    let knowledge = knowledge_for_events(&state, &events).await?;
    let related_count = if let (Some(pet), Some(event)) = (&selected_pet, events.first()) {
        db::count_related(&state.db, user.household_id, pet.id, &event.concept).await?
    } else {
        0
    };
    let shares = db::list_shares(&state.db, user.household_id).await?;
    let weights = match &selected_pet {
        Some(pet) => db::list_weights(&state.db, user.household_id, pet.id).await?,
        None => Vec::new(),
    };
    let lab_reports = match &selected_pet {
        Some(pet) => db::list_lab_reports(&state.db, user.household_id, pet.id).await?,
        None => Vec::new(),
    };
    render(&ConsoleTemplate {
        user,
        pets,
        selected_pet,
        events,
        knowledge,
        related_count,
        shares,
        weights,
        lab_reports,
        new_share_path: None,
        capture_message: None,
        capture_error: None,
    })
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
    Ok(Redirect::to(&format!("/?pet={id}")))
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
    Form(form): Form<WeightForm>,
) -> Result<Redirect, AppError> {
    if db::get_pet(&state.db, user.household_id, form.pet_id)
        .await?
        .is_none()
    {
        return Err(AppError::not_found());
    }
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
    Ok(Redirect::to(&format!("/?pet={}", form.pet_id)))
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
    Ok(Redirect::to("/"))
}

#[derive(Deserialize)]
struct CaptureForm {
    message: String,
    selected_pet_id: Option<i64>,
}

async fn capture(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Form(form): Form<CaptureForm>,
) -> Result<Response, AppError> {
    let message = clean_required(&form.message, 1000, "Observation")?.to_owned();
    let pets = db::list_pets(&state.db, user.household_id).await?;
    let names: Vec<_> = pets.iter().map(|pet| pet.name.clone()).collect();
    let proposal = match state.agent.propose(&message, &names).await {
        Ok(value) => value,
        Err(error) => {
            let selected_pet =
                selected_from(&state, user.household_id, &pets, form.selected_pet_id).await?;
            let events = db::list_events(
                &state.db,
                user.household_id,
                selected_pet.as_ref().map(|pet| pet.id),
                50,
            )
            .await?;
            return render_status(
                &AgentTimelineTemplate {
                    selected_pet,
                    events,
                    capture_message: None,
                    capture_error: Some(error.to_string()),
                },
                StatusCode::UNPROCESSABLE_ENTITY,
            );
        }
    };
    let pet = db::find_pet_by_name(&state.db, user.household_id, &proposal.pet_name)
        .await?
        .ok_or_else(|| AppError::validation("That pet no longer exists."))?;
    let received_at = Utc::now();
    let occurred_at = state.agent.occurred_at(&proposal, received_at);
    db::create_health_event(
        &state.db,
        user.household_id,
        &user.audit_actor(),
        &pet,
        &proposal,
        &message,
        occurred_at,
        "owner_agent",
    )
    .await?;
    let events = db::list_events(&state.db, user.household_id, Some(pet.id), 50).await?;
    render_status(
        &AgentTimelineTemplate {
            selected_pet: Some(pet),
            events,
            capture_message: Some(format!("Saved: {}", proposal.summary)),
            capture_error: None,
        },
        StatusCode::OK,
    )
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
    let events = db::list_events(
        &state.db,
        user.household_id,
        selected_pet.as_ref().map(|pet| pet.id),
        50,
    )
    .await?;
    render(&AgentTimelineTemplate {
        selected_pet,
        events,
        capture_message: Some("Event removed from the timeline.".into()),
        capture_error: None,
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

async fn knowledge_for_events(
    state: &AppState,
    events: &[HealthEvent],
) -> Result<Option<KnowledgeArticle>, AppError> {
    match events.first() {
        Some(event) => Ok(db::get_knowledge(&state.db, &event.concept).await?),
        None => Ok(None),
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
#[template(path = "console.html")]
struct ConsoleTemplate {
    user: UserAccount,
    pets: Vec<Pet>,
    selected_pet: Option<Pet>,
    events: Vec<HealthEvent>,
    knowledge: Option<KnowledgeArticle>,
    related_count: u64,
    shares: Vec<ShareGrant>,
    weights: Vec<WeightEntry>,
    lab_reports: Vec<LabReport>,
    new_share_path: Option<String>,
    capture_message: Option<String>,
    capture_error: Option<String>,
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
    selected_pet: Option<Pet>,
    events: Vec<HealthEvent>,
    capture_message: Option<String>,
    capture_error: Option<String>,
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
