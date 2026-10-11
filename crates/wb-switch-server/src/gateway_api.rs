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

use wb_switch_core::modules::{
    account, config, egress, gateway, proxy, refresh, risk, variant::WbVariant,
};

/// 本模块的全部路由。
pub fn router() -> Router {
    Router::new()
        .route("/api/gateway", get(api_gateway_snapshot))
        .route("/api/gateway/config", post(api_gateway_config))
        .route("/api/gateway/usage/clear", post(api_gateway_clear_usage))
        .route("/api/gateway/models", get(api_gateway_models))
        .route("/api/risk/check", post(api_risk_check))
        .route("/api/egress", get(api_egress_snapshot))
        .route("/api/egress/detect", post(api_egress_detect))
        .route("/api/egress/assign", post(api_egress_assign))
        .route("/api/proxy/test", post(api_proxy_test))
        .route("/api/account-proxy", post(api_account_proxy))
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
// 出口 IP 与账号级代理
// ---------------------------------------------------------------------------

/// GET /api/egress —— 当前出口 IP + 该 IP 上挂了哪些号 + 换 IP 向导。
async fn api_egress_snapshot() -> Response {
    json_ok(egress::snapshot())
}

/// POST /api/egress/detect —— 检测出口 IP。body: `{"force": true}` 绕过 5 分钟缓存。
///
/// 默认 `force = false`：页面每次挂载都打一遍探测接口既慢又容易被限流。向导第 3 步
/// （换完 IP 要确认）必须传 `force`，否则会读到换之前的缓存值，把「没换成功」看成成功。
async fn api_egress_detect(body: Option<Json<Value>>) -> Response {
    let force = body
        .as_ref()
        .and_then(|Json(value)| value.get("force"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    json_ok(egress::detect(force).await)
}

/// POST /api/egress/assign —— 把出口 IP 记到一批账号上。
///
/// body: `{"accountIds": ["id"...], "ip": "1.2.3.4"}`；`ip` 缺省时用当前检测到的 IP
/// （向导第 4 步「一键确认」走这条）。
async fn api_egress_assign(Json(body): Json<Value>) -> Response {
    let ids: Vec<String> = body
        .get("accountIds")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        return json_err("accountIds 不能为空", StatusCode::BAD_REQUEST);
    }
    let explicit = body
        .get("ip")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let result = match explicit {
        Some(ip) => egress::assign(&ids, ip),
        None => egress::assign_current(&ids),
    };
    json_ok(result)
}

/// POST /api/proxy/test —— 代理连通性自检。body: `{"proxy": "http://host:port"}`。
///
/// 返回体里的 `proxy` 是**掩码后**的地址，不回显密码。
async fn api_proxy_test(Json(body): Json<Value>) -> Response {
    let raw = body.get("proxy").and_then(|v| v.as_str()).unwrap_or("");
    json_ok(proxy::test(raw).await)
}

/// POST /api/account-proxy —— 设置/清除某个账号的出口代理。
///
/// body: `{"id": "...", "proxy": "http://host:port"}`；`proxy` 传空串表示改回直连。
/// 地址先过 [`proxy::normalize`] 再落库——存进去的一定是能直接喂给 reqwest 的形态，
/// 免得把「用户以为配好了」的坏地址留到真正发请求时才炸。
async fn api_account_proxy(Json(body): Json<Value>) -> Response {
    let Some(id) = body
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return json_err("缺少账号 id", StatusCode::BAD_REQUEST);
    };
    let raw = body.get("proxy").and_then(|v| v.as_str()).unwrap_or("");
    let normalized = match proxy::normalize(raw) {
        Ok(value) => value,
        Err(message) => return json_err(message, StatusCode::BAD_REQUEST),
    };

    let Some(mut account) = account::load_accounts()
        .into_iter()
        .find(|acc| account::get_str(acc, "id").as_deref() == Some(id))
    else {
        return json_err("账号不存在", StatusCode::NOT_FOUND);
    };
    if let Some(obj) = account.as_object_mut() {
        match &normalized {
            Some(value) => {
                obj.insert("proxy".to_string(), json!(value));
            }
            // 直连：删字段而不是写空串，让账号库保持「没配过代理」的原样。
            None => {
                obj.remove("proxy");
            }
        }
    }
    if let Err(error) = account::upsert_account(&account) {
        return json_err(
            format!("保存失败: {error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    }
    json_ok(json!({
        "id": id,
        "proxy": normalized.clone().unwrap_or_default(),
        "masked": normalized.as_deref().map(proxy::mask).unwrap_or_default(),
    }))
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
///
/// `Err` 里装的是 `Box<Response>`：`Response` 本体 100+ 字节，直接塞进 `Result`
/// 会让每个 `Ok` 路径都跟着搬这么大一块（clippy `result_large_err`）。失败路径
/// 本来就走不了几次，多一次堆分配毫无代价。
fn gate_or_error(
    variant: WbVariant,
    headers: &HeaderMap,
) -> Result<gateway::RealmConfig, Box<Response>> {
    let cfg = gateway::realm_config(variant);
    if !cfg.enabled {
        return Err(Box::new(openai_json(
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
        )));
    }
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if !gateway::check_auth(auth, &cfg.key) {
        return Err(Box::new(openai_json(
            401,
            gateway::openai_error(
                "API Key 不正确（注意：国内版和国际版是两把不同的密钥）",
                401,
                "invalid_api_key",
                "authentication_error",
                None,
            ),
        )));
    }
    Ok(cfg)
}

async fn v1_models(headers: HeaderMap, variant: WbVariant) -> Response {
    if let Err(response) = gate_or_error(variant, &headers) {
        return *response;
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
        Err(response) => return *response,
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
