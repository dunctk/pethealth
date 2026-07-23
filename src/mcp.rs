use crate::{
    AppState, db,
    domain::{ProposedEvent, UserAccount},
};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
        Ok(user) => handle(&state, &user, request).await,
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
    db::resolve_session(&state.db, token)
        .await
        .ok()
        .flatten()
        .ok_or((
            StatusCode::UNAUTHORIZED,
            "The session is missing, expired, or revoked.",
        ))
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
        {"name":"import_blood_tests","description":"OCR new PDF and image files from the configured blood-test folder using Mistral OCR 4.","inputSchema":{"type":"object","properties":{} }},
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
