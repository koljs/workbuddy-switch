//! OpenAI 兼容网关 + 风控体检的 HTTP 层。
//!
//! 单独成文件，避免与 `api.rs` 里已有的业务路由相互挤压；由 `api.rs` 的
//! [`crate::api::router`] 通过 `.merge()` 挂上去。
//!
//! 路由分两类：
//! - `/api/gateway*`、`/api/risk/check`：给 webui 用的管理接口（无鉴权，本机回环）
//! - `/v1*`、`/v1-global*`：**对外**的 OpenAI 兼容接口，用网关密钥鉴权
//!
//! 注意 `/v1` 与 `/v1-global` 是两套完全独立的网关：各自的密钥、号池、每日上限、
//! 用量统计。填了哪个地址就一定在打哪一边的号，不会串。

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::{Json, Query};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::Router;
use serde_json::{json, Value};

use wb_switch_core::modules::{account, config, gateway, refresh, risk, variant::WbVariant};

/// 本模块的全部路由。
pub fn router() -> Router {
    Router::new()
        .route("/api/gateway", get(api_gateway_snapshot))
        .route("/api/gateway/config", post(api_gateway_config))
        .route("/api/gateway/usage/clear", post(api_gateway_clear_usage))
        .route("/api/gateway/models", get(api_gateway_models))
        .route("/api/risk/check", post(api_risk_check))
        .route("/v1/models", get(v1_cn_models))
        .route("/v1/chat/completions", post(v1_cn_chat))
        .route("/v1-global/models", get(v1_global_models))
        .route("/v1-global/chat/completions", post(v1_global_chat))
        // 客户端把子路径打错时（如 /v1/chat/completion 少了 s），回一个看得懂的
        // 提示，别丢 HTML 404 让顾客对着页面猜。
        .route("/v1/*rest", any(v1_unknown_cn))
        .route("/v1-global/*rest", any(v1_unknown_global))
}

fn json_ok(value: Value) -> Response {
    Json(value).into_response()
}

fn json_err(message: impl Into<String>, status: StatusCode) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

fn openai_json(status: u16, body: Value) -> Response {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (code, Json(body)).into_response()
}

fn sse_response(payload: String) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(Body::from(payload))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

// ---------------------------------------------------------------------------
// 管理接口（webui 用）
// ---------------------------------------------------------------------------

/// GET /api/gateway —— 两个档位的网关快照。
async fn api_gateway_snapshot() -> Response {
    json_ok(gateway::payload())
}

/// POST /api/gateway/config —— 保存某个档位的网关配置。
///
/// body: `{"realm": "cn" | "global", "enabled": bool, "key": str, "dailyLimit": int, "pool": [id]}`
/// 未给的字段保持不变；`key` 传空串或 `"regenerate"` 表示换一把新的。
async fn api_gateway_config(Json(body): Json<Value>) -> Response {
    let variant = gateway::gw_realm(body.get("realm").and_then(|v| v.as_str()));
    let mut patch = serde_json::Map::new();
    for key in ["enabled", "key", "dailyLimit", "pool"] {
        if let Some(value) = body.get(key) {
            patch.insert(key.to_string(), value.clone());
        }
    }
    gateway::update_config(variant, &Value::Object(patch));
    json_ok(gateway::payload())
}

/// POST /api/gateway/usage/clear —— 只清该档位的用量，不动另一个版本。
async fn api_gateway_clear_usage(body: Option<Json<Value>>) -> Response {
    let variant = gateway::gw_realm(
        body.as_ref()
            .and_then(|Json(value)| value.get("realm"))
            .and_then(|v| v.as_str()),
    );
    gateway::clear_usage(variant);
    json_ok(gateway::payload())
}

/// GET /api/gateway/models?realm=cn —— 界面预览可用模型（走该档位号池里的第一个号）。
async fn api_gateway_models(Query(params): Query<HashMap<String, String>>) -> Response {
    let variant = gateway::gw_realm(params.get("realm").map(|raw| raw.as_str()));

    let Some(account) = gateway::entries(variant).into_iter().next() else {
        return json_err(
            format!(
                "没有可用的{}账号（先在账号页保存一个）",
                gateway::realm_label(variant)
            ),
            StatusCode::BAD_REQUEST,
        );
    };
    match gateway::fetch_models(&account).await {
        Ok(models) => {
            let list = models.as_array().cloned().unwrap_or_default();
            json_ok(gateway::models_payload(&list))
        }
        Err(error) => json_err(error.message, StatusCode::BAD_GATEWAY),
    }
}

/// POST /api/risk/check —— 风控体检。body: `{"realm": "cn" | "global" | "all"}`（缺省=全部）。
async fn api_risk_check(body: Option<Json<Value>>) -> Response {
    let realm = body
        .as_ref()
        .and_then(|Json(value)| value.get("realm"))
        .and_then(|v| v.as_str())
        .map(|raw| raw.to_string());
    json_ok(risk::check_all(realm.as_deref()).await)
}

// ---------------------------------------------------------------------------
// OpenAI 兼容接口
// ---------------------------------------------------------------------------

async fn v1_cn_models(headers: HeaderMap) -> Response {
    v1_models(headers, WbVariant::Cn).await
}

async fn v1_global_models(headers: HeaderMap) -> Response {
    v1_models(headers, WbVariant::Ai).await
}

async fn v1_cn_chat(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    v1_chat(headers, body, WbVariant::Cn).await
}

async fn v1_global_chat(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    v1_chat(headers, body, WbVariant::Ai).await
}

async fn v1_unknown_cn() -> Response {
    v1_unknown(WbVariant::Cn)
}

async fn v1_unknown_global() -> Response {
    v1_unknown(WbVariant::Ai)
}

/// 未知子路径的统一提示。
fn v1_unknown(variant: WbVariant) -> Response {
    let prefix = match variant {
        WbVariant::Cn => "/v1",
        WbVariant::Ai => "/v1-global",
    };
    openai_json(
        404,
        gateway::openai_error(
            &format!(
                "没有这个接口。可用的是 {prefix}/models（模型列表）与 {prefix}/chat/completions（对话）"
            ),
            404,
            "unknown_endpoint",
            "invalid_request_error",
            None,
        ),
    )
}

/// 网关未启用 / 密钥不对这类前置校验。
fn gate_or_error(
    variant: WbVariant,
    headers: &HeaderMap,
) -> Result<gateway::RealmConfig, Response> {
    let cfg = gateway::realm_config(variant);
    if !cfg.enabled {
        return Err(openai_json(
            503,
            gateway::openai_error(
                &format!(
                    "{}网关未启用（在管理器界面里打开）",
                    gateway::realm_label(variant)
                ),
                503,
                "gateway_disabled",
                "service_unavailable",
                None,
            ),
        ));
    }
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if !gateway::check_auth(auth, &cfg.key) {
        return Err(openai_json(
            401,
            gateway::openai_error(
                "API Key 不正确（注意：国内版和国际版是两把不同的密钥）",
                401,
                "invalid_api_key",
                "authentication_error",
                None,
            ),
        ));
    }
    Ok(cfg)
}

async fn v1_models(headers: HeaderMap, variant: WbVariant) -> Response {
    if let Err(response) = gate_or_error(variant, &headers) {
        return response;
    }
    let Some(account) = gateway::entries(variant).into_iter().next() else {
        return openai_json(
            502,
            gateway::openai_error(
                &format!(
                    "没有可用的{}账号（先在管理器里保存一个）",
                    gateway::realm_label(variant)
                ),
                502,
                "upstream_error",
                "upstream_error",
                None,
            ),
        );
    };
    match gateway::fetch_models(&account).await {
        Ok(models) => {
            let list = models.as_array().cloned().unwrap_or_default();
            json_ok(gateway::models_payload(&list))
        }
        Err(error) => openai_json(error.status, error.to_body()),
    }
}

/// 对话主流程：挑号 → 换号重试 → 流式或聚合返回。
async fn v1_chat(headers: HeaderMap, body: Value, variant: WbVariant) -> Response {
    let cfg = match gate_or_error(variant, &headers) {
        Ok(cfg) => cfg,
        Err(response) => return response,
    };

    if cfg.daily_limit > 0 && gateway::today_count(variant) >= cfg.daily_limit {
        return openai_json(
            429,
            gateway::openai_error(
                "今日请求已达上限，明天自动重置",
                429,
                "rate_limit_exceeded",
                "rate_limit_error",
                None,
            ),
        );
    }

    let prepared = match gateway::prepare_body(&body) {
        Ok(value) => value,
        Err(error) => return openai_json(error.status, error.to_body()),
    };
    let stream = prepared
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let model_hint = prepared
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        .to_string();

    let candidates = gateway::entries(variant);
    if candidates.is_empty() {
        return openai_json(
            502,
            gateway::openai_error(
                &format!(
                    "没有可用的{}账号（先在管理器里保存一个）",
                    gateway::realm_label(variant)
                ),
                502,
                "upstream_error",
                "upstream_error",
                None,
            ),
        );
    }

    let by_id: HashMap<String, Value> = candidates
        .iter()
        .filter_map(|acc| account::get_str(acc, "id").map(|id| (id, acc.clone())))
        .collect();
    let ids: Vec<String> = by_id.keys().cloned().collect();
    let ordered = gateway::pool().order(&ids);

    let mut last_error: Option<gateway::GatewayError> = None;
    for id in ordered.into_iter().take(gateway::MAX_ACCOUNT_TRIES) {
        let Some(account) = by_id.get(&id) else {
            continue;
        };
        if !gateway::pool().acquire(&id) {
            continue;
        }

        let checkin_cfg = config::load_checkin_config();
        let working = refresh::ensure_fresh_token(account.clone(), &checkin_cfg).await;
        let conversation_id = gateway::new_hex32();
        let request_id = gateway::new_hex32();
        let mut note: Option<String> = None;

        let outcome = gateway::open_chat(
            &working,
            &prepared,
            &mut note,
            &conversation_id,
            &request_id,
        )
        .await;

        match outcome {
            Ok((raw, _)) => {
                let frames = gateway::iter_raw_frames(&raw);
                if stream {
                    let payload = gateway::iter_openai_sse(&raw, &model_hint).concat();
                    gateway::pool().release(&id);
                    gateway::pool().note_success(&id);
                    gateway::record_usage(variant, 0, false);
                    return sse_response(payload);
                }
                match gateway::aggregate(&frames, &model_hint) {
                    Ok(value) => {
                        let tokens = value
                            .get("usage")
                            .and_then(|u| u.get("total_tokens"))
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        gateway::pool().release(&id);
                        gateway::pool().note_success(&id);
                        gateway::record_usage(variant, tokens, false);
                        return json_ok(value);
                    }
                    Err(error) => {
                        gateway::pool().release(&id);
                        gateway::pool().note_error(&id);
                        gateway::record_usage(variant, 0, true);
                        last_error = Some(error);
                    }
                }
            }
            Err(error) => {
                gateway::pool().release(&id);
                gateway::pool().note_error(&id);
                gateway::record_usage(variant, 0, true);
                last_error = Some(error);
            }
        }
    }

    let error = last_error.unwrap_or_else(|| {
        gateway::GatewayError::new(
            502,
            "upstream_error",
            "所有候选账号都失败了（可先跑一次风控体检看看哪些号还能用）",
            "upstream_error",
        )
    });
    openai_json(error.status, error.to_body())
}
