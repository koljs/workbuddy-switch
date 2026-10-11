//! 分享包的 HTTP 层。
//!
//! 单独成文件，与 `gateway_api.rs` 同构：由 `api.rs` 的 [`crate::api::router`]
//! 通过 `.merge()` 挂上去。
//!
//! 四个路由：
//! - `POST /api/share`          生成分享包（写进 `<store>/分享包/`）
//! - `GET  /api/share/history`  历史记录
//! - `GET  /api/share/download` 取包体（webui 直接下载）
//! - `POST /api/share/import`   导入一个包（等价于对方双击 .bat）
//!
//! 这些都是管理接口，和 `/api/gateway*` 一样**只在回环上跑、不做鉴权**——分享包里
//! 是账号凭证，暴露到公网等于把账号送人。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::{Json, Query};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};

use wb_switch_core::modules::share;

pub fn router() -> Router {
    Router::new()
        .route("/api/share", post(api_share_create))
        .route("/api/share/history", get(api_share_history))
        .route("/api/share/download", get(api_share_download))
        .route("/api/share/import", post(api_share_import))
}

fn ok(value: Value) -> Response {
    Json(value).into_response()
}

fn err(message: impl Into<String>, status: StatusCode) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// 生成分享包。`id` 必填——一次一个号，避免「多选账号打成一个包」后凭证混在一起。
async fn api_share_create(Json(body): Json<Value>) -> Response {
    let Some(id) = body
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return err("缺少账号 id", StatusCode::BAD_REQUEST);
    };
    match share::build_package(id, None) {
        Ok(meta) => ok(json!({ "ok": true, "package": meta })),
        Err(message) => err(message, StatusCode::BAD_REQUEST),
    }
}

async fn api_share_history() -> Response {
    ok(json!({ "history": share::load_history() }))
}

/// 取包体给 webui 下载。
///
/// `file` 只允许是**纯文件名**：带 `/`、`\` 或 `..` 一律拒掉，否则这就成了一个
/// 「读服务器任意文件」的接口。
///
/// 用 `HashMap` 而不是自定义 `Deserialize` 结构体：本 crate 没有直接依赖 `serde`，
/// 和 `gateway_api.rs` / `api.rs` 里其它查询接口保持一致。
async fn api_share_download(Query(query): Query<HashMap<String, String>>) -> Response {
    let name = query.get("file").map(String::as_str).unwrap_or("").trim();
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || Path::new(name).components().count() != 1
    {
        return err("非法的文件名", StatusCode::BAD_REQUEST);
    }
    if !name.starts_with(share::PACKAGE_PREFIX) || !name.ends_with(share::PACKAGE_EXT) {
        return err("只能下载分享包", StatusCode::BAD_REQUEST);
    }
    let path: PathBuf = share::share_dir().join(name);
    let Ok(bytes) = std::fs::read(&path) else {
        return err("分享包不存在（可能已被删除）", StatusCode::NOT_FOUND);
    };
    let size = bytes.len();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/zip")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{name}\""),
        )
        .header(header::CONTENT_LENGTH, size.to_string())
        .body(Body::from(bytes))
        .unwrap_or_else(|_| err("构造响应失败", StatusCode::INTERNAL_SERVER_ERROR))
}

/// 导入分享包。`path` 指向解压后的目录或 zip 本身。
async fn api_share_import(Json(body): Json<Value>) -> Response {
    let Some(path) = body
        .get("path")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return err("缺少 path", StatusCode::BAD_REQUEST);
    };
    let switch = body
        .get("switch")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    match share::import_package(Path::new(path), switch) {
        Ok(result) => ok(json!({ "ok": true, "result": result })),
        Err(message) => err(message, StatusCode::BAD_REQUEST),
    }
}
