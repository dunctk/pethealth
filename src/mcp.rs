use crate::{
    AppState, db,
    domain::{ProposedEvent, UserAccount},
};
use axum::{
    Json,
    extract::{Form, Query, State},
    http::{HeaderMap, StatusCode, Uri, header},
    response::{Html, IntoResponse, Redirect, Response},
};
use base64::Engine;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

#[derive(Debug, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Option<Value>,
    result: Option<Value>,
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

pub async fn endpoint(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<RpcRequest>,
) -> impl IntoResponse {
    let id = request.id.clone();
    if request.jsonrpc != "2.0" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"jsonrpc":"2.0", "id": id, "error":{"code":-32600,"message":"jsonrpc must be 2.0."}})),
        ).into_response();
    }
    let response = match authenticate(&state, &headers).await {
        Ok(user) => {
            if request.id.is_none() {
                return StatusCode::NO_CONTENT.into_response();
            }
            handle(&state, &user, request).await
        }
        Err((status, message)) => {
            return (
                status,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                Json(json!({
                    "jsonrpc":"2.0", "id": id, "error": {"code": -32001, "message": message}
                })),
            )
                .into_response();
        }
    };
    (StatusCode::OK, Json(response)).into_response()
}

async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<UserAccount, (StatusCode, &'static str)> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let token = bearer.or_else(|| cookie_token(state, headers));
    let Some(token) = token else {
        return Err((StatusCode::UNAUTHORIZED, "Sign in is required."));
    };
    if let Some(user) = db::resolve_oauth_access_token(&state.db, token)
        .await
        .ok()
        .flatten()
    {
        return Ok(user);
    }
    db::resolve_session(&state.db, token)
        .await
        .ok()
        .flatten()
        .ok_or((
            StatusCode::UNAUTHORIZED,
            "The session is missing, expired, or revoked.",
        ))
}

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub client_name: Option<String>,
    pub redirect_uris: Vec<String>,
}

pub async fn register_client(
    State(state): State<AppState>,
    Json(request): Json<RegisterRequest>,
) -> Result<Json<Value>, Response> {
    if request.redirect_uris.is_empty() || request.redirect_uris.len() > 10 {
        return Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "redirect_uris is required.",
        ));
    }
    if request.redirect_uris.iter().any(|uri| {
        uri.is_empty()
            || uri.contains('#')
            || uri
                .parse::<Uri>()
                .ok()
                .is_none_or(|parsed| parsed.scheme().is_none() || parsed.authority().is_none())
    }) {
        return Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "Each redirect URI must be absolute and must not contain a fragment.",
        ));
    }
    let client_id = format!("pethealth_{}", crate::auth::new_session_token());
    let client_name = request.client_name.unwrap_or_else(|| "MCP client".into());
    db::create_oauth_client(&state.db, &client_id, &client_name, &request.redirect_uris)
        .await
        .map_err(internal_response)?;
    Ok(Json(json!({
        "client_id":client_id,
        "client_name":client_name,
        "redirect_uris":request.redirect_uris,
        "grant_types":["authorization_code","refresh_token"],
        "response_types":["code"],
        "token_endpoint_auth_method":"none"
    })))
}

#[derive(Debug, Deserialize)]
pub struct DeviceAuthorizationForm {
    pub client_id: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub scope: Option<String>,
}

pub async fn start_device_authorization(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<DeviceAuthorizationForm>,
) -> Result<Json<Value>, Response> {
    validate_device_client(&state, &form).await?;
    let device_code = crate::auth::new_session_token();
    let user_code = new_user_code();
    db::create_oauth_device_code(
        &state.db,
        &crate::auth::token_hash(&device_code),
        &crate::auth::token_hash(&normalize_user_code(&user_code)),
        &form.client_id,
        &form.code_challenge,
    )
    .await
    .map_err(internal_response)?;
    let origin = public_origin(&state, &headers);
    Ok(Json(json!({
        "device_code": device_code,
        "user_code": user_code,
        "verification_uri": format!("{origin}/oauth/device"),
        "verification_uri_complete": format!("{origin}/oauth/device?user_code={}", urlencoding::encode(&user_code)),
        "expires_in": 600,
        "interval": 5,
        "scope": form.scope.unwrap_or_else(|| "pethealth".into())
    })))
}

#[derive(Debug, Deserialize, Default)]
pub struct DevicePageQuery {
    pub user_code: Option<String>,
}

pub async fn device_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DevicePageQuery>,
) -> Result<Response, Response> {
    let Some(raw_code) = query.user_code.as_deref() else {
        return Ok(Html(device_code_entry_html(None)).into_response());
    };
    let user_code = normalize_user_code(raw_code);
    if user_code.len() != 10
        || !db::oauth_device_user_code_exists(&state.db, &crate::auth::token_hash(&user_code))
            .await
            .map_err(internal_response)?
    {
        return Ok(Html(device_code_entry_html(Some(
            "That code is invalid or expired.",
        )))
        .into_response());
    }
    if cookie_user(&state, &headers).await.is_none() {
        let next = format!("/oauth/device?user_code={}", urlencoding::encode(raw_code));
        return Ok(
            Redirect::to(&format!("/login?next={}", urlencoding::encode(&next))).into_response(),
        );
    }
    Ok(Html(device_consent_html(&user_code)).into_response())
}

#[derive(Debug, Deserialize)]
pub struct DeviceVerifyForm {
    pub user_code: String,
}

pub async fn verify_device_code(Form(form): Form<DeviceVerifyForm>) -> Result<Redirect, Response> {
    let user_code = normalize_user_code(&form.user_code);
    if user_code.len() != 10 {
        return Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "Enter the code shown by the MCP client.",
        ));
    }
    Ok(Redirect::to(&format!(
        "/oauth/device?user_code={}",
        urlencoding::encode(&form.user_code)
    )))
}

#[derive(Debug, Deserialize)]
pub struct DeviceApproveForm {
    pub user_code: String,
}

pub async fn approve_device_code(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<DeviceApproveForm>,
) -> Result<Response, Response> {
    let user = cookie_user(&state, &headers)
        .await
        .ok_or_else(|| oauth_error(StatusCode::UNAUTHORIZED, "Sign in is required."))?;
    let user_code = normalize_user_code(&form.user_code);
    let approved =
        db::approve_oauth_device_code(&state.db, &crate::auth::token_hash(&user_code), user.id)
            .await
            .map_err(internal_response)?;
    if !approved {
        return Ok(Html(device_code_entry_html(Some(
            "That code is invalid, expired, or already approved.",
        )))
        .into_response());
    }
    Ok(Html("<!doctype html><html lang=\"en\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Pet Health connected</title><link rel=\"stylesheet\" href=\"/static/app.css\"><body class=\"auth-page\"><main class=\"auth-shell\"><section class=\"auth-panel\"><div class=\"eyebrow\">PET HEALTH / MCP</div><h1>Connected.</h1><p>The MCP client can finish signing in now. You can close this window.</p></section></main></body></html>").into_response())
}

async fn validate_device_client(
    state: &AppState,
    form: &DeviceAuthorizationForm,
) -> Result<(), Response> {
    if form.code_challenge_method != "S256" || form.code_challenge.len() < 43 {
        return Err(oauth_error(StatusCode::BAD_REQUEST, "Use S256 PKCE."));
    }
    if db::oauth_client_redirects(&state.db, &form.client_id)
        .await
        .map_err(internal_response)?
        .is_none()
    {
        return Err(oauth_error(StatusCode::BAD_REQUEST, "Unknown client_id."));
    }
    Ok(())
}

fn new_user_code() -> String {
    let raw = crate::auth::new_session_token().to_ascii_uppercase();
    format!("{}-{}", &raw[..5], &raw[5..10])
}

fn normalize_user_code(code: &str) -> String {
    code.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_uppercase())
        .collect()
}

fn device_code_entry_html(error: Option<&str>) -> String {
    let message = error
        .map(|value| {
            format!(
                "<div class=\"auth-error\" role=\"alert\">{}</div>",
                escape(value)
            )
        })
        .unwrap_or_default();
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Connect Pet Health</title><link rel=\"stylesheet\" href=\"/static/app.css\"></head><body class=\"auth-page\"><main class=\"auth-shell\"><section class=\"auth-panel\"><div class=\"eyebrow\">PET HEALTH / MCP</div><h1>Connect an MCP client</h1><p>Enter the one-time code shown in your terminal.</p>{message}<form method=\"post\" action=\"/oauth/device/verify\" class=\"auth-form\"><label>Connection code<input required autofocus name=\"user_code\" autocomplete=\"one-time-code\" autocapitalize=\"characters\" placeholder=\"ABCDE-FGHIJ\"></label><button class=\"button primary\" type=\"submit\">Continue</button></form></section></main></body></html>"
    )
}

fn device_consent_html(user_code: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Connect Pet Health</title><link rel=\"stylesheet\" href=\"/static/app.css\"></head><body class=\"auth-page\"><main class=\"auth-shell\"><section class=\"auth-panel\"><div class=\"eyebrow\">PET HEALTH / MCP</div><h1>Connect this client?</h1><p>The MCP client will be able to read and update the signed-in household through Pet Health.</p><form method=\"post\" action=\"/oauth/device/approve\" class=\"auth-form\"><input type=\"hidden\" name=\"user_code\" value=\"{}\"><button class=\"button primary\" type=\"submit\">Allow access</button></form><p class=\"auth-switch\"><a href=\"/\">Cancel</a></p></section></main></body></html>",
        escape(user_code)
    )
}

#[derive(Debug, Deserialize)]
pub struct AuthorizeQuery {
    pub response_type: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub state: Option<String>,
}

pub async fn authorize(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AuthorizeQuery>,
) -> Result<Response, Response> {
    validate_authorize_request(&state, &query).await?;
    let user = cookie_user(&state, &headers).await;
    let Some(_) = user else {
        let next = format!(
            "/oauth/authorize?response_type={}&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256{}",
            urlencoding::encode(&query.response_type),
            urlencoding::encode(&query.client_id),
            urlencoding::encode(&query.redirect_uri),
            urlencoding::encode(&query.code_challenge),
            query
                .state
                .as_deref()
                .map(|state| format!("&state={}", urlencoding::encode(state)))
                .unwrap_or_default()
        );
        return Ok(
            Redirect::to(&format!("/login?next={}", urlencoding::encode(&next))).into_response(),
        );
    };
    let state_field = query.state.clone().unwrap_or_default();
    Ok(Html(format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Connect Pet Health</title><link rel=\"stylesheet\" href=\"/static/app.css\"></head><body class=\"auth-page\"><main class=\"auth-shell\"><section class=\"auth-panel\"><div class=\"eyebrow\">PET HEALTH / MCP</div><h1>Connect your pet history?</h1><p>This agent will be able to read and update the signed-in household through Pet Health.</p><form method=\"post\" action=\"/oauth/authorize/approve\" class=\"auth-form\"><input type=\"hidden\" name=\"client_id\" value=\"{}\"><input type=\"hidden\" name=\"redirect_uri\" value=\"{}\"><input type=\"hidden\" name=\"code_challenge\" value=\"{}\"><input type=\"hidden\" name=\"state\" value=\"{}\"><button class=\"button primary\" type=\"submit\">Allow access</button></form><p class=\"auth-switch\"><a href=\"/\">Cancel</a></p></section></main></body></html>",
        escape(&query.client_id), escape(&query.redirect_uri), escape(&query.code_challenge), escape(&state_field)
    )).into_response())
}

pub async fn approve_authorize(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ApproveForm>,
) -> Result<Response, Response> {
    let query = AuthorizeQuery {
        response_type: "code".into(),
        client_id: form.client_id.clone(),
        redirect_uri: form.redirect_uri.clone(),
        code_challenge: form.code_challenge.clone(),
        code_challenge_method: "S256".into(),
        state: (!form.state.is_empty()).then_some(form.state.clone()),
    };
    validate_authorize_request(&state, &query).await?;
    let user = cookie_user(&state, &headers)
        .await
        .ok_or_else(|| oauth_error(StatusCode::UNAUTHORIZED, "Sign in is required."))?;
    let code = crate::auth::new_session_token();
    db::create_oauth_code(
        &state.db,
        &crate::auth::token_hash(&code),
        &form.client_id,
        user.id,
        &form.redirect_uri,
        &form.code_challenge,
    )
    .await
    .map_err(internal_response)?;
    let mut location = format!(
        "{}{}code={}",
        form.redirect_uri,
        if form.redirect_uri.contains('?') {
            '&'
        } else {
            '?'
        },
        urlencoding::encode(&code)
    );
    if !form.state.is_empty() {
        location.push_str(&format!("&state={}", urlencoding::encode(&form.state)));
    }
    Ok(Redirect::to(&location).into_response())
}

#[derive(Debug, Deserialize)]
pub struct ApproveForm {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    state: String,
}

#[derive(Debug, Deserialize)]
pub struct TokenForm {
    grant_type: String,
    client_id: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    device_code: Option<String>,
}

pub async fn token(
    State(state): State<AppState>,
    Form(form): Form<TokenForm>,
) -> Result<Json<Value>, Response> {
    let tokens =
        match form.grant_type.as_str() {
            "authorization_code" => {
                let code = form
                    .code
                    .as_deref()
                    .ok_or_else(|| oauth_error(StatusCode::BAD_REQUEST, "code is required."))?;
                let redirect_uri = form.redirect_uri.as_deref().ok_or_else(|| {
                    oauth_error(StatusCode::BAD_REQUEST, "redirect_uri is required.")
                })?;
                let verifier = form.code_verifier.as_deref().ok_or_else(|| {
                    oauth_error(StatusCode::BAD_REQUEST, "code_verifier is required.")
                })?;
                let Some((user_id, challenge)) = db::redeem_oauth_code(
                    &state.db,
                    &crate::auth::token_hash(code),
                    &form.client_id,
                    redirect_uri,
                )
                .await
                .map_err(internal_response)?
                else {
                    return Err(oauth_error(
                        StatusCode::BAD_REQUEST,
                        "The authorization code is invalid or expired.",
                    ));
                };
                let digest = Sha256::digest(verifier.as_bytes());
                let computed = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
                if computed.as_bytes().ct_eq(challenge.as_bytes()).unwrap_u8() != 1 {
                    return Err(oauth_error(
                        StatusCode::BAD_REQUEST,
                        "PKCE verification failed.",
                    ));
                }
                db::create_oauth_tokens(&state.db, &form.client_id, user_id)
                    .await
                    .map_err(internal_response)?
            }
            "refresh_token" => {
                let refresh = form.refresh_token.as_deref().ok_or_else(|| {
                    oauth_error(StatusCode::BAD_REQUEST, "refresh_token is required.")
                })?;
                db::refresh_oauth_tokens(&state.db, refresh, &form.client_id)
                    .await
                    .map_err(internal_response)?
                    .ok_or_else(|| {
                        oauth_error(
                            StatusCode::BAD_REQUEST,
                            "The refresh token is invalid or expired.",
                        )
                    })?
            }
            "urn:ietf:params:oauth:grant-type:device_code" => {
                let device_code = form.device_code.as_deref().ok_or_else(|| {
                    oauth_error(StatusCode::BAD_REQUEST, "device_code is required.")
                })?;
                let verifier = form.code_verifier.as_deref().ok_or_else(|| {
                    oauth_error(StatusCode::BAD_REQUEST, "code_verifier is required.")
                })?;
                let Some((Some(user_id), challenge)) = db::oauth_device_code_state(
                    &state.db,
                    &crate::auth::token_hash(device_code),
                    &form.client_id,
                )
                .await
                .map_err(internal_response)?
                else {
                    if db::oauth_device_code_state(
                        &state.db,
                        &crate::auth::token_hash(device_code),
                        &form.client_id,
                    )
                    .await
                    .map_err(internal_response)?
                    .is_some()
                    {
                        return Err(oauth_error_code(
                            StatusCode::BAD_REQUEST,
                            "authorization_pending",
                            "Approve the connection in a browser first.",
                        ));
                    }
                    return Err(oauth_error_code(
                        StatusCode::BAD_REQUEST,
                        "invalid_grant",
                        "The device code is invalid or expired.",
                    ));
                };
                let digest = Sha256::digest(verifier.as_bytes());
                let computed = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
                if computed.as_bytes().ct_eq(challenge.as_bytes()).unwrap_u8() != 1 {
                    return Err(oauth_error_code(
                        StatusCode::BAD_REQUEST,
                        "invalid_grant",
                        "PKCE verification failed.",
                    ));
                }
                if !db::consume_oauth_device_code(
                    &state.db,
                    &crate::auth::token_hash(device_code),
                    &form.client_id,
                    user_id,
                )
                .await
                .map_err(internal_response)?
                {
                    return Err(oauth_error_code(
                        StatusCode::BAD_REQUEST,
                        "authorization_pending",
                        "Approve the connection in a browser first.",
                    ));
                }
                db::create_oauth_tokens(&state.db, &form.client_id, user_id)
                    .await
                    .map_err(internal_response)?
            }
            _ => {
                return Err(oauth_error(
                    StatusCode::BAD_REQUEST,
                    "Unsupported grant_type.",
                ));
            }
        };
    Ok(Json(
        json!({"token_type":"Bearer","access_token":tokens.0,"refresh_token":tokens.1,"expires_in":tokens.2,"scope":"pethealth"}),
    ))
}

pub async fn protected_resource_metadata(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Json<Value> {
    let origin = public_origin(&state, &headers);
    Json(
        json!({"resource":format!("{origin}/mcp"),"authorization_servers":[origin],"scopes_supported":["pethealth"],"bearer_methods_supported":["header"]}),
    )
}

pub async fn authorization_server_metadata(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Json<Value> {
    let origin = public_origin(&state, &headers);
    Json(
        json!({"issuer":origin,"authorization_endpoint":format!("{origin}/oauth/authorize"),"device_authorization_endpoint":format!("{origin}/oauth/device"),"token_endpoint":format!("{origin}/oauth/token"),"registration_endpoint":format!("{origin}/oauth/register"),"response_types_supported":["code"],"grant_types_supported":["authorization_code","refresh_token","urn:ietf:params:oauth:grant-type:device_code"],"code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["none"],"scopes_supported":["pethealth"]}),
    )
}

async fn validate_authorize_request(
    state: &AppState,
    query: &AuthorizeQuery,
) -> Result<(), Response> {
    if query.response_type != "code"
        || query.code_challenge_method != "S256"
        || query.code_challenge.len() < 43
    {
        return Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "Use response_type=code with S256 PKCE.",
        ));
    }
    let Some(redirects) = db::oauth_client_redirects(&state.db, &query.client_id)
        .await
        .map_err(internal_response)?
    else {
        return Err(oauth_error(StatusCode::BAD_REQUEST, "Unknown client_id."));
    };
    if !redirects.iter().any(|uri| uri == &query.redirect_uri) {
        return Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "redirect_uri is not registered.",
        ));
    }
    Ok(())
}

async fn cookie_user(state: &AppState, headers: &HeaderMap) -> Option<UserAccount> {
    let token = cookie_token(state, headers)?;
    db::resolve_session(&state.db, token).await.ok().flatten()
}

fn public_origin(state: &AppState, headers: &HeaderMap) -> String {
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

fn oauth_error(status: StatusCode, message: &str) -> Response {
    oauth_error_code(status, "invalid_request", message)
}
fn oauth_error_code(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error":code,"error_description":message})),
    )
        .into_response()
}
fn internal_response(error: impl std::fmt::Display) -> Response {
    oauth_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string())
}
fn escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn cookie_token<'a>(state: &AppState, headers: &'a HeaderMap) -> Option<&'a str> {
    let name = if state.config.production {
        "__Host-pethealth_session"
    } else {
        "pethealth_session"
    };
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

async fn handle(state: &AppState, user: &UserAccount, request: RpcRequest) -> RpcResponse {
    let id = request.id.clone();
    let result = match request.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion":"2025-11-25",
            "capabilities":{"tools":{"listChanged":false}},
            "serverInfo":{"name":"pethealth","version":env!("CARGO_PKG_VERSION")}
        })),
        "notifications/initialized" => Ok(Value::Null),
        "tools/list" => Ok(tool_list()),
        "tools/call" => call_tool(state, user, request.params).await,
        _ => Err((-32601, format!("Unknown method: {}", request.method))),
    };
    match result {
        Ok(result) => RpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        },
        Err((code, message)) => RpcResponse {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError { code, message }),
        },
    }
}

fn tool_list() -> Value {
    json!({"tools":[
        {"name":"list_pets","description":"List the pets in the signed-in household.","inputSchema":{"type":"object","properties":{}},"annotations":{"readOnlyHint":true}},
        {"name":"get_pet_timeline","description":"Read active health events for one pet. The original wording is included.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"limit":{"type":"integer","minimum":1,"maximum":100}},"required":["pet_id"]},"annotations":{"readOnlyHint":true}},
        {"name":"get_health_context","description":"Read practical care context for a health concept. This is not a diagnosis.","inputSchema":{"type":"object","properties":{"concept":{"type":"string"}},"required":["concept"]},"annotations":{"readOnlyHint":true}},
        {"name":"get_weight_history","description":"Read weight measurements over time for one pet.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"}},"required":["pet_id"]},"annotations":{"readOnlyHint":true}},
        {"name":"get_blood_test_history","description":"Read imported blood-test values for one pet. Original reports remain stored for review.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"}},"required":["pet_id"]},"annotations":{"readOnlyHint":true}},
        {"name":"add_pet","description":"Add a pet to the signed-in household.","inputSchema":{"type":"object","properties":{"name":{"type":"string"},"species":{"type":"string"},"breed":{"type":"string"},"weight_kg":{"type":"number"}},"required":["name","species"]}},
        {"name":"record_weight","description":"Save a weight measurement for a pet.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"weight_kg":{"type":"number"},"measured_at":{"type":"string","description":"YYYY-MM-DD"},"note":{"type":"string"}},"required":["pet_id","weight_kg","measured_at"]}},
        {"name":"upload_blood_test","description":"Store one PDF or image in this household's private area and import it with Mistral OCR 4.","inputSchema":{"type":"object","properties":{"filename":{"type":"string"},"content_base64":{"type":"string","description":"The PDF or image bytes encoded as base64."}},"required":["filename","content_base64"]}},
        {"name":"import_blood_tests","description":"OCR new PDF and image files from this household's private blood-test folder using Mistral OCR 4.","inputSchema":{"type":"object","properties":{} }},
        {"name":"record_health_event","description":"Record one observation from the user's wording. The server chooses the timestamp and preserves the original wording.","inputSchema":{"type":"object","properties":{"message":{"type":"string"}},"required":["message"]}},
        {"name":"undo_health_event","description":"Undo an active health event in the signed-in household.","inputSchema":{"type":"object","properties":{"event_id":{"type":"integer"}},"required":["event_id"]}}
    ]})
}

async fn call_tool(
    state: &AppState,
    user: &UserAccount,
    params: Value,
) -> Result<Value, (i32, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((-32602, "Tool name is required.".into()))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let value = match name {
        "list_pets" => serde_json::to_value(
            db::list_pets(&state.db, user.household_id)
                .await
                .map_err(internal)?,
        )
        .map_err(internal)?,
        "get_pet_timeline" => {
            let pet_id = integer(&args, "pet_id")?;
            if db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .is_none()
            {
                return Err((-32602, "That pet is not in this household.".into()));
            }
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(50)
                .clamp(1, 100);
            serde_json::to_value(
                db::list_events(&state.db, user.household_id, Some(pet_id), limit)
                    .await
                    .map_err(internal)?,
            )
            .map_err(internal)?
        }
        "get_health_context" => {
            let concept = string(&args, "concept")?;
            serde_json::to_value(
                db::get_knowledge(&state.db, &concept)
                    .await
                    .map_err(internal)?,
            )
            .map_err(internal)?
        }
        "get_weight_history" => {
            let pet_id = integer(&args, "pet_id")?;
            if db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .is_none()
            {
                return Err((-32602, "That pet is not in this household.".into()));
            }
            serde_json::to_value(
                db::list_weights(&state.db, user.household_id, pet_id)
                    .await
                    .map_err(internal)?,
            )
            .map_err(internal)?
        }
        "get_blood_test_history" => {
            let pet_id = integer(&args, "pet_id")?;
            if db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .is_none()
            {
                return Err((-32602, "That pet is not in this household.".into()));
            }
            serde_json::to_value(
                db::list_lab_reports(&state.db, user.household_id, pet_id)
                    .await
                    .map_err(internal)?,
            )
            .map_err(internal)?
        }
        "add_pet" => {
            let name = string(&args, "name")?;
            let species = string(&args, "species")?;
            let id = db::create_pet(
                &state.db,
                user.household_id,
                &user.audit_actor(),
                &name,
                &species,
                args.get("breed").and_then(Value::as_str),
                args.get("weight_kg").and_then(Value::as_f64),
            )
            .await
            .map_err(internal)?;
            json!({"pet_id":id,"message":"Pet added."})
        }
        "record_weight" => {
            let pet_id = integer(&args, "pet_id")?;
            if db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .is_none()
            {
                return Err((-32602, "That pet is not in this household.".into()));
            }
            let weight = args
                .get("weight_kg")
                .and_then(Value::as_f64)
                .ok_or((-32602, "weight_kg is required.".into()))?;
            if !(0.01..=500.0).contains(&weight) {
                return Err((-32602, "weight_kg must be between 0.01 and 500.".into()));
            }
            let date = string(&args, "measured_at")?;
            let measured_at = if date.len() == 10 {
                format!("{date}T12:00:00Z")
            } else {
                date
            };
            let id = db::create_weight(
                &state.db,
                user.household_id,
                &user.audit_actor(),
                pet_id,
                weight,
                &measured_at,
                args.get("note").and_then(Value::as_str),
            )
            .await
            .map_err(internal)?;
            json!({"weight_id":id,"message":"Weight saved."})
        }
        "upload_blood_test" => {
            let filename = string(&args, "filename")?;
            let content = string(&args, "content_base64")?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(content)
                .map_err(|_| (-32602, "content_base64 is invalid.".into()))?;
            crate::ocr::store_upload(&state.config, user.household_id, &filename, &bytes)
                .await
                .map_err(internal)?;
            let pets = db::list_pets(&state.db, user.household_id)
                .await
                .map_err(internal)?;
            let imported = crate::ocr::import_directory(
                &state.config,
                &state.db,
                user.household_id,
                &user.audit_actor(),
                &pets,
            )
            .await
            .map_err(internal)?;
            serde_json::to_value(imported.iter().map(|item| json!({"filename":item.filename,"report_id":item.report_id,"message":item.message})).collect::<Vec<_>>()).map_err(internal)?
        }
        "import_blood_tests" => {
            let pets = db::list_pets(&state.db, user.household_id)
                .await
                .map_err(internal)?;
            let imported = crate::ocr::import_directory(
                &state.config,
                &state.db,
                user.household_id,
                &user.audit_actor(),
                &pets,
            )
            .await
            .map_err(internal)?;
            serde_json::to_value(imported.iter().map(|item| json!({"filename":item.filename,"report_id":item.report_id,"message":item.message})).collect::<Vec<_>>()).map_err(internal)?
        }
        "record_health_event" => {
            let message = string(&args, "message")?;
            let pets = db::list_pets(&state.db, user.household_id)
                .await
                .map_err(internal)?;
            let names = pets.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
            let proposal: ProposedEvent = state
                .agent
                .propose(&message, &names)
                .await
                .map_err(|e| (-32602, e.to_string()))?;
            let pet = db::find_pet_by_name(&state.db, user.household_id, &proposal.pet_name)
                .await
                .map_err(internal)?
                .ok_or((-32602, "That pet is not in this household.".into()))?;
            let event_id = db::create_health_event(
                &state.db,
                user.household_id,
                &user.audit_actor(),
                &pet,
                &proposal,
                &message,
                state.agent.occurred_at(&proposal, Utc::now()),
                "mcp",
            )
            .await
            .map_err(internal)?;
            json!({"event_id":event_id,"pet":pet.name,"summary":proposal.summary,"message":"Health event recorded."})
        }
        "undo_health_event" => {
            json!({"undone":db::undo_event(&state.db, user.household_id, &user.audit_actor(), integer(&args, "event_id")?).await.map_err(internal)?})
        }
        _ => return Err((-32601, format!("Unknown tool: {name}"))),
    };
    Ok(
        json!({"content":[{"type":"text","text":serde_json::to_string_pretty(&value).map_err(internal)?}],"structuredContent":value,"isError":false}),
    )
}

fn integer(args: &Value, key: &str) -> Result<i64, (i32, String)> {
    args.get(key)
        .and_then(Value::as_i64)
        .ok_or((-32602, format!("{key} must be an integer.")))
}
fn string(args: &Value, key: &str) -> Result<String, (i32, String)> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
        .ok_or((-32602, format!("{key} is required.")))
}
fn internal(error: impl std::fmt::Display) -> (i32, String) {
    (-32603, error.to_string())
}
