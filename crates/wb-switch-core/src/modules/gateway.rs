//! OpenAI 兼容网关：把账号库变成 `/v1` 与 `/v1-global` 两个标准 OpenAI 接口。
//!
//! 设计目标与 `WBMultiLogin` 的网关一致，但按本仓库的架构重写：
//! - 账号来源复用 `account::load_accounts`（同一份 `~/.wb-switch/accounts.json`）
//! - token 续期复用 `refresh::ensure_fresh_token`
//! - 上游协议常量（域名 / 路径 / 出站请求头 / 版本号）按官方桌面客户端形态固定，
//!   缺指纹会被上游 `403 code 10085`，因此这里逐个显式声明，不做"看起来更简洁"的合并。
//!
//! 国内版与国际版是**两套完全独立**的网关：各自的密钥、号池、每日上限、用量统计。
//! 两个版本可以同时启用，互不影响。
//!
//! 号池策略：按"最久没用过的优先"排序；正在服务其它请求的号排后；冷却中的号垫底
//! （仍列出，便于全部冷却时兜底）。单次请求最多尝试 [`MAX_ACCOUNT_TRIES`] 个账号。

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::modules::account::{get_str, load_accounts, variant_of};
use crate::modules::config::{http_request_raw, now_ms, store_dir};
use crate::modules::refresh::ensure_fresh_token;
use crate::modules::variant::WbVariant;

// ---------------------------------------------------------------------------
// 上游协议常量（对齐官方桌面客户端；改动前先确认上游是否已变更）
// ---------------------------------------------------------------------------

/// 国内版对话域（refresh 也走这里）。
pub const CN_CHAT_BASE: &str = "https://copilot.tencent.com";
/// 国内版 billing 域（积分 / 签到）。
pub const CN_BILLING_BASE: &str = "https://www.codebuddy.cn";
/// 国际版统一域。
pub const GLOBAL_BASE: &str = "https://www.workbuddy.ai";

/// 官方桌面客户端版本，参与 UA 与 `X-IDE-Version` 指纹。
pub const CLIENT_VERSION: &str = "5.5.4";
/// 官方 CLI 版本，参与 UA 指纹。
pub const CLI_VERSION: &str = "2.137.1";
/// 网关自称版本（写进 `X-IDE-Name` 之外的头部，便于上游侧排查）。
pub const GATEWAY_VERSION: &str = "wb-multilogin-gateway/1.0";
/// 出站 `X-IDE-Version` 使用的桌面版本。
pub const DESKTOP_VERSION: &str = "5.5.6";

const CHAT_PATH_V2: &str = "/v2/chat/completions";
const CHAT_PATH_CONSOLE: &str = "/console/chat/completions";
const MODEL_LIST_PATHS: [&str; 2] = [
    "/v2/enterprises/personal/models",
    "/console/enterprises/personal/models",
];

/// 模型目录缓存时长（毫秒）。目录变动很慢，缓存避免每次开页都打上游。
const MODEL_CACHE_TTL_MS: i64 = 600_000;
/// 入站请求体上限（字节）。超大请求直接拒绝，不进上游。
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// 单次请求最多尝试几个账号（换号重试）。
pub const MAX_ACCOUNT_TRIES: usize = 4;
/// 账号失败后的冷却秒数。
pub const COOLDOWN_SECONDS: i64 = 90;
/// 上游流最多读多少帧，防止坏流把内存吃满。
const MAX_UPSTREAM_FRAMES: usize = 200_000;
/// 未指定 max_tokens 时给上游的默认值。官方模型是推理型，太小会只思考不出话。
pub const DEFAULT_MAX_TOKENS: i64 = 8192;
/// 上游"不是账号坏了、也不是请求坏了"的风控 / 指纹类错误码。
pub const UPSTREAM_RISK_CODES: [i64; 4] = [10085, 11101, 11128, 11140];
/// 上游明确的账号级风控码（体检判 `risk` 只认它）。
pub const UPSTREAM_RISK_CONTROL_CODE: i64 = 11140;
/// 上游支持的最高推理档位集合。
pub const REASONING_EFFORTS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
/// 错误文案截断长度。
const TRUNCATE_LEN: usize = 300;

// ---------------------------------------------------------------------------
// 档位（realm）
// ---------------------------------------------------------------------------

/// 网关档位：界面 / URL / 历史数据里可能是空串或大小写混乱，这里统一收敛。
pub fn gw_realm(raw: Option<&str>) -> WbVariant {
    match raw.unwrap_or("").trim().to_lowercase().as_str() {
        "global" | "ai" | "intl" => WbVariant::Ai,
        _ => WbVariant::Cn,
    }
}

/// 档位标识（写盘 / 传给前端用）。
pub fn realm_str(variant: WbVariant) -> &'static str {
    match variant {
        WbVariant::Cn => "cn",
        WbVariant::Ai => "global",
    }
}

/// 界面展示名。
pub fn realm_label(variant: WbVariant) -> &'static str {
    match variant {
        WbVariant::Cn => "国内版",
        WbVariant::Ai => "国际版",
    }
}

/// 按档位返回 (chat_base, billing_base, origin)。
pub fn bases(variant: WbVariant) -> (&'static str, &'static str, &'static str) {
    match variant {
        WbVariant::Cn => (CN_CHAT_BASE, CN_BILLING_BASE, "https://www.codebuddy.cn"),
        WbVariant::Ai => (GLOBAL_BASE, GLOBAL_BASE, "https://www.workbuddy.ai"),
    }
}

/// 对话端点候选路径：国际版先 `/console/...` 再回落 `/v2/...`，国内版只有 `/v2/...`。
pub fn chat_paths(variant: WbVariant) -> Vec<&'static str> {
    match variant {
        WbVariant::Ai => vec![CHAT_PATH_CONSOLE, CHAT_PATH_V2],
        WbVariant::Cn => vec![CHAT_PATH_V2],
    }
}

/// 官方桌面端出站 UA（三段式）。国际版第二段品牌不同，送错会 403。
pub fn client_ua(variant: WbVariant) -> String {
    let platform = match variant {
        WbVariant::Ai => "WorkBuddy AI",
        WbVariant::Cn => "WorkBuddy",
    };
    format!("WorkBuddy/{CLIENT_VERSION} {platform}/{CLIENT_VERSION} CLI/{CLI_VERSION}")
}

// ---------------------------------------------------------------------------
// 稳定派生 ID
// ---------------------------------------------------------------------------

/// 按 `uid + 用途` 稳定派生 36 位 hex（同一账号每次相同，绝不能用随机值）。
pub fn stable_id(uid: &str, purpose: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("wb2a:{purpose}:{uid}").as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:x}");
    hex.chars().take(36).collect()
}

/// 每请求唯一的 32 位 hex（并发安全，带纳秒与线程 id 熵）。
pub fn new_hex32() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tid = format!("{:?}", std::thread::current().id());
    let mut hasher = Sha256::new();
    hasher.update(format!("{nanos}:{tid}").as_bytes());
    let digest = hasher.finalize();
    format!("{digest:x}").chars().take(32).collect()
}

/// 生成一把新的网关密钥。
pub fn new_api_key() -> String {
    format!("sk-wb-{}", new_hex32())
}

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 网关内部错误：既能翻成 OpenAI 错误体，也能带上游 code 供体检归类。
#[derive(Debug, Clone)]
pub struct GatewayError {
    pub status: u16,
    pub code: String,
    pub message: String,
    pub err_type: String,
    pub param: Option<Value>,
}

impl GatewayError {
    pub fn new(status: u16, code: &str, message: impl Into<String>, err_type: &str) -> Self {
        Self {
            status,
            code: code.to_string(),
            message: message.into(),
            err_type: err_type.to_string(),
            param: None,
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request_error", message, "invalid_request_error")
    }

    /// OpenAI 规范错误体。
    pub fn to_body(&self) -> Value {
        openai_error(&self.message, self.status, &self.code, &self.err_type, self.param.clone())
    }
}

/// 按 OpenAI 规范组装错误体。
pub fn openai_error(message: &str, status: u16, code: &str, err_type: &str, param: Option<Value>) -> Value {
    json!({
        "error": {
            "message": truncate(message, TRUNCATE_LEN),
            "type": err_type,
            "code": code,
            "param": param.unwrap_or(Value::Null),
            "status": status,
        }
    })
}

/// 截断过长文案，避免把上游整段 HTML 丢给客户端。
pub fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}…")
}

/// 从上游响应体里取业务 code（数字或数字字符串都认）。
pub fn upstream_code(body: &Value) -> Option<i64> {
    let raw = body.get("code").or_else(|| {
        body.get("error")
            .and_then(|e| e.get("code"))
    })?;
    raw.as_i64()
        .or_else(|| raw.as_str().and_then(|s| s.trim().parse().ok()))
}

/// 把上游错误映射成网关错误。风控类 code 单列，方便体检与前端提示。
pub fn map_upstream_error(status: u16, body: &Value) -> GatewayError {
    let code = upstream_code(body);
    let message = body
        .get("displayMsg")
        .or_else(|| body.get("message"))
        .or_else(|| body.get("msg"))
        .and_then(|v| v.as_str())
        .unwrap_or("上游返回了错误")
        .to_string();

    if let Some(code) = code {
        if UPSTREAM_RISK_CODES.contains(&code) {
            let hint = if code == UPSTREAM_RISK_CONTROL_CODE {
                "该账号已被上游风控（换 IP / 换 token 均无效，需在官方客户端内反馈申诉）"
            } else {
                "上游风控拦截（请求指纹或账号状态异常）"
            };
            return GatewayError::new(
                502,
                &format!("upstream_risk_control:{code}"),
                format!("{message}（{hint}）"),
                "upstream_error",
            );
        }
    }

    let (mapped_status, err_type) = match status {
        401 | 403 => (401, "authentication_error"),
        _ => (502, "upstream_error"),
    };
    GatewayError::new(
        mapped_status,
        "upstream_error",
        message,
        err_type,
    )
}

/// 校验客户端带来的密钥。定长比较，避免时序侧信道。
pub fn check_auth(auth_header: Option<&str>, expected_key: &str) -> bool {
    let Some(raw) = auth_header else {
        return false;
    };
    let token = raw
        .trim()
        .strip_prefix("Bearer ")
        .or_else(|| raw.trim().strip_prefix("bearer "))
        .unwrap_or(raw.trim())
        .trim();
    const_eq(token, expected_key)
}

/// 常量时间字符串比较。
pub fn const_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// 配置与用量（落盘）
// ---------------------------------------------------------------------------

/// 单个档位的网关配置。
#[derive(Debug, Clone)]
pub struct RealmConfig {
    pub enabled: bool,
    pub key: String,
    /// 0 = 不限。
    pub daily_limit: i64,
    /// 参与轮转的账号 id；空 = 该档位全部账号。
    pub pool: Vec<String>,
}

impl RealmConfig {
    fn default_for(variant: WbVariant) -> Self {
        let _ = variant;
        Self {
            enabled: false,
            key: new_api_key(),
            daily_limit: 0,
            pool: Vec::new(),
        }
    }
}

/// 网关配置文件路径。
pub fn gateway_config_file() -> std::path::PathBuf {
    store_dir().join("gateway.json")
}

/// 用量文件路径。
pub fn gateway_usage_file() -> std::path::PathBuf {
    store_dir().join("gateway-usage.json")
}

fn read_json_file(path: &std::path::Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null)
}

fn write_json_file(path: &std::path::Path, value: &Value) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(value) {
        let _ = std::fs::write(path, text);
    }
}

/// 读全量网关配置（两个档位）。
pub fn load_config() -> Value {
    let stored = read_json_file(&gateway_config_file());
    let mut out = serde_json::Map::new();
    for variant in [WbVariant::Cn, WbVariant::Ai] {
        let name = realm_str(variant);
        let node = stored.get(name);
        let fallback = RealmConfig::default_for(variant);
        out.insert(
            name.to_string(),
            json!({
                "enabled": node.and_then(|n| n.get("enabled")).and_then(|v| v.as_bool()).unwrap_or(fallback.enabled),
                "key": node
                    .and_then(|n| n.get("key"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_string())
                    .unwrap_or(fallback.key),
                "dailyLimit": node.and_then(|n| n.get("dailyLimit")).and_then(|v| v.as_i64()).unwrap_or(0),
                "pool": node
                    .and_then(|n| n.get("pool"))
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect::<Vec<_>>())
                    .unwrap_or_default(),
            }),
        );
    }
    Value::Object(out)
}

/// 读某个档位的配置（缺省补齐）。
pub fn realm_config(variant: WbVariant) -> RealmConfig {
    let all = load_config();
    let node = all.get(realm_str(variant));
    let fallback = RealmConfig::default_for(variant);
    RealmConfig {
        enabled: node
            .and_then(|n| n.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(fallback.enabled),
        key: node
            .and_then(|n| n.get("key"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string())
            .unwrap_or(fallback.key),
        daily_limit: node
            .and_then(|n| n.get("dailyLimit"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        pool: node
            .and_then(|n| n.get("pool"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
    }
}

/// 局部更新某个档位的配置，返回更新后的全量配置。
pub fn update_config(variant: WbVariant, patch: &Value) -> Value {
    let mut all = load_config();
    let name = realm_str(variant);
    let mut node = all.get(name).cloned().unwrap_or_else(|| json!({}));
    if let Some(obj) = node.as_object_mut() {
        for (key, value) in patch.as_object().into_iter().flatten() {
            match key.as_str() {
                "enabled" => {
                    if let Some(flag) = value.as_bool() {
                        obj.insert("enabled".to_string(), json!(flag));
                    }
                }
                "key" => {
                    // 允许"重新生成"：传空串或 "regenerate" 时换一把新的。
                    let next = value.as_str().unwrap_or("").trim();
                    let resolved = if next.is_empty() || next == "regenerate" {
                        new_api_key()
                    } else {
                        next.to_string()
                    };
                    obj.insert("key".to_string(), json!(resolved));
                }
                "dailyLimit" => {
                    if let Some(limit) = value.as_i64() {
                        obj.insert("dailyLimit".to_string(), json!(limit.max(0)));
                    }
                }
                "pool" => {
                    let ids: Vec<String> = value
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    obj.insert("pool".to_string(), json!(ids));
                }
                _ => {}
            }
        }
    }
    if let Some(root) = all.as_object_mut() {
        root.insert(name.to_string(), node);
    }
    write_json_file(&gateway_config_file(), &all);
    all
}

/// 用量结构：按档位分账。
fn empty_usage() -> Value {
    json!({
        "date": "",
        "today": 0,
        "total": 0,
        "tokens": 0,
        "failures": 0,
        "lastUsedAt": 0,
    })
}

/// 读全量用量（两个档位各自一份）。
pub fn load_usage() -> Value {
    let stored = read_json_file(&gateway_usage_file());
    let mut out = serde_json::Map::new();
    for variant in [WbVariant::Cn, WbVariant::Ai] {
        let name = realm_str(variant);
        let node = stored.get(name).cloned().unwrap_or_else(empty_usage);
        out.insert(name.to_string(), node);
    }
    Value::Object(out)
}

fn today_key() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// 今日已用次数（跨日自动归零，且会落盘，重启不白送额度）。
pub fn today_count(variant: WbVariant) -> i64 {
    let usage = load_usage();
    let node = usage.get(realm_str(variant)).cloned().unwrap_or_else(empty_usage);
    let date = node.get("date").and_then(|v| v.as_str()).unwrap_or("");
    if date != today_key() {
        return 0;
    }
    node.get("today").and_then(|v| v.as_i64()).unwrap_or(0)
}

/// 记一次请求结果。
pub fn record_usage(variant: WbVariant, tokens: i64, failed: bool) {
    let mut all = load_usage();
    let name = realm_str(variant);
    let mut node = all.get(name).cloned().unwrap_or_else(empty_usage);
    let today = today_key();
    let is_new_day = node.get("date").and_then(|v| v.as_str()).unwrap_or("") != today;
    if let Some(obj) = node.as_object_mut() {
        obj.insert("date".to_string(), json!(today));
        obj.insert(
            "today".to_string(),
            json!(if is_new_day {
                1
            } else {
                obj.get("today").and_then(|v| v.as_i64()).unwrap_or(0) + 1
            }),
        );
        obj.insert(
            "total".to_string(),
            json!(obj.get("total").and_then(|v| v.as_i64()).unwrap_or(0) + 1),
        );
        if tokens > 0 {
            obj.insert(
                "tokens".to_string(),
                json!(obj.get("tokens").and_then(|v| v.as_i64()).unwrap_or(0) + tokens),
            );
        }
        if failed {
            obj.insert(
                "failures".to_string(),
                json!(obj.get("failures").and_then(|v| v.as_i64()).unwrap_or(0) + 1),
            );
        }
        obj.insert("lastUsedAt".to_string(), json!(now_ms()));
    }
    if let Some(root) = all.as_object_mut() {
        root.insert(name.to_string(), node);
    }
    write_json_file(&gateway_usage_file(), &all);
}

/// 清空某个档位的用量。
pub fn clear_usage(variant: WbVariant) {
    let mut all = load_usage();
    if let Some(root) = all.as_object_mut() {
        root.insert(realm_str(variant).to_string(), empty_usage());
    }
    write_json_file(&gateway_usage_file(), &all);
}

// ---------------------------------------------------------------------------
// 号池
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct PoolEntry {
    in_flight: u32,
    last_used: i64,
    cooldown_until: i64,
}

/// 多账号轮转池：挑号 / 冷却 / 统计。纯内存，跨进程重启即清空（用量另存）。
#[derive(Default)]
pub struct Pool {
    inner: Mutex<HashMap<String, PoolEntry>>,
}

impl Pool {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, PoolEntry>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 按"最久没用过的优先"排号；正在服务其它请求的号排后；冷却中的垫底。
    pub fn order(&self, ids: &[String]) -> Vec<String> {
        let now = now_ms();
        let mut guard = self.lock();
        let mut ranked: Vec<(u8, i64, String)> = ids
            .iter()
            .map(|id| {
                let entry = guard.entry(id.clone()).or_default();
                let cooling = entry.cooldown_until > now;
                let tier = if cooling {
                    2
                } else if entry.in_flight > 0 {
                    1
                } else {
                    0
                };
                (tier, entry.last_used, id.clone())
            })
            .collect();
        ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        ranked.into_iter().map(|(_, _, id)| id).collect()
    }

    /// 认领一个号（标记在服务中）。
    pub fn acquire(&self, id: &str) -> bool {
        let mut guard = self.lock();
        let entry = guard.entry(id.to_string()).or_default();
        if entry.cooldown_until > now_ms() {
            return false;
        }
        entry.in_flight += 1;
        entry.last_used = now_ms();
        true
    }

    /// 归还一个号。
    pub fn release(&self, id: &str) {
        let mut guard = self.lock();
        if let Some(entry) = guard.get_mut(id) {
            entry.in_flight = entry.in_flight.saturating_sub(1);
        }
    }

    /// 记一次成功：清冷却。
    pub fn note_success(&self, id: &str) {
        let mut guard = self.lock();
        let entry = guard.entry(id.to_string()).or_default();
        entry.cooldown_until = 0;
        entry.last_used = now_ms();
    }

    /// 记一次失败：进入冷却。
    pub fn note_error(&self, id: &str) {
        let mut guard = self.lock();
        let entry = guard.entry(id.to_string()).or_default();
        entry.cooldown_until = now_ms() + COOLDOWN_SECONDS * 1000;
    }

    /// 冷却中的号数量。
    pub fn cooling_count(&self, ids: &[String]) -> usize {
        let now = now_ms();
        let guard = self.lock();
        ids.iter()
            .filter(|id| {
                guard
                    .get(*id)
                    .map(|e| e.cooldown_until > now)
                    .unwrap_or(false)
            })
            .count()
    }
}

static POOL: OnceLock<Pool> = OnceLock::new();

/// 全局号池。
pub fn pool() -> &'static Pool {
    POOL.get_or_init(Pool::default)
}

// ---------------------------------------------------------------------------
// 账号候选
// ---------------------------------------------------------------------------

/// 该档位的候选号（**还没做风控剔除**）：按号池设置过滤。
pub fn candidates(variant: WbVariant) -> Vec<Value> {
    let cfg = realm_config(variant);
    let selected: HashSet<String> = cfg.pool.iter().cloned().collect();
    load_accounts()
        .into_iter()
        .filter(|acc| variant_of(acc) == variant)
        .filter(|acc| {
            if selected.is_empty() {
                return true;
            }
            get_str(acc, "id")
                .map(|id| selected.contains(&id))
                .unwrap_or(false)
        })
        .collect()
}

/// 真正参与轮转的账号 = 候选号剔除"刚被体检判为风控"的那些。
///
/// 为什么在挑号这层就剔：被风控的号发对话必 403，留着只会让顾客白等一轮重试。
/// 全部被剔光时退回候选全集 —— 宁可拿它试一把，也不要直接报"没有可用账号"。
pub fn entries(variant: WbVariant) -> Vec<Value> {
    let all = candidates(variant);
    let healthy: Vec<Value> = all
        .iter()
        .filter(|acc| !crate::modules::risk::is_risk_blocked(acc))
        .cloned()
        .collect();
    if healthy.is_empty() {
        all
    } else {
        healthy
    }
}

// ---------------------------------------------------------------------------
// 模型目录
// ---------------------------------------------------------------------------

fn model_cache() -> &'static Mutex<HashMap<String, (i64, Value)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (i64, Value)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 拉真实模型目录（含能力旗标）。带 10 分钟缓存。
pub async fn fetch_models(account: &Value) -> Result<Value, GatewayError> {
    let variant = variant_of(account);
    let cache_key = realm_str(variant).to_string();
    if let Ok(guard) = model_cache().lock() {
        if let Some((stamp, cached)) = guard.get(&cache_key) {
            if now_ms() - *stamp < MODEL_CACHE_TTL_MS {
                return Ok(cached.clone());
            }
        }
    }

    let refreshed = ensure_fresh_token(account.clone(), &crate::modules::config::load_checkin_config()).await;
    let uid = get_str(&refreshed, "uid").unwrap_or_default();
    let token = get_str(&refreshed, "access_token").unwrap_or_default();
    let (chat_base, _, _) = bases(variant);

    let mut headers = common_headers(&refreshed);
    headers.insert("Authorization".to_string(), format!("Bearer {token}"));
    headers.insert("Accept".to_string(), "application/json".to_string());

    let mut last_error: Option<GatewayError> = None;
    for path in MODEL_LIST_PATHS {
        let url = format!("{chat_base}{path}");
        let (status, _, text) = http_request_raw(&url, "GET", None, Some(&headers), None, true).await;
        if status == 404 || status == 405 {
            continue;
        }
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !(200..300).contains(&status) {
            last_error = Some(map_upstream_error(status, &body));
            continue;
        }
        let list = body
            .get("data")
            .and_then(|v| v.as_array())
            .cloned()
            .or_else(|| body.as_array().cloned())
            .unwrap_or_default();
        if list.is_empty() {
            last_error = Some(GatewayError::new(
                502,
                "upstream_error",
                "上游模型目录为空",
                "upstream_error",
            ));
            continue;
        }
        let _ = uid;
        let payload = Value::Array(list);
        if let Ok(mut guard) = model_cache().lock() {
            guard.insert(cache_key, (now_ms(), payload.clone()));
        }
        return Ok(payload);
    }

    Err(last_error.unwrap_or_else(|| {
        GatewayError::new(502, "upstream_error", "模型目录请求失败", "upstream_error")
    }))
}

/// 组装 `GET /v1/models` 响应体（OpenAI 规范 + 能力附加字段）。
pub fn models_payload(models: &[Value]) -> Value {
    let mut data = Vec::new();
    for item in models {
        let id = item
            .get("id")
            .or_else(|| item.get("model"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if id.is_empty() {
            continue;
        }
        let vendor = item
            .get("vendor")
            .and_then(|v| v.as_str())
            .unwrap_or("workbuddy");
        let tags = item.get("tags").cloned().unwrap_or_else(|| json!([]));
        let reasoning = item
            .get("reasoning")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let supported: Vec<String> = item
            .get("supportedEfforts")
            .or_else(|| item.get("supported_efforts"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        data.push(json!({
            "id": id,
            "object": "model",
            "created": 0,
            "owned_by": vendor,
            "tags": tags,
            "reasoning": reasoning,
            "supported_efforts": supported,
        }));
    }
    json!({"object": "list", "data": data})
}

// ---------------------------------------------------------------------------
// 出站请求头（风控指纹，缺了会被上游 403 code 10085）
// ---------------------------------------------------------------------------

/// 所有 API 共享头。
pub fn common_headers(account: &Value) -> HashMap<String, String> {
    let variant = variant_of(account);
    let uid = get_str(account, "uid").unwrap_or_default();
    let (_, _, origin) = bases(variant);
    let mut headers = HashMap::new();
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers.insert("Accept".to_string(), "application/json".to_string());
    headers.insert("X-Requested-With".to_string(), "XMLHttpRequest".to_string());
    headers.insert("Origin".to_string(), origin.to_string());
    headers.insert("Referer".to_string(), format!("{origin}/"));
    headers.insert("User-Agent".to_string(), client_ua(variant));
    headers.insert("Accept-Language".to_string(), "zh-CN,en-US;q=0.9".to_string());
    headers.insert("X-User-Id".to_string(), uid.clone());
    headers.insert("X-Machine-ID".to_string(), stable_id(&uid, "machine"));
    headers.insert("X-Session-ID".to_string(), stable_id(&uid, "session"));
    if let Some(eid) = get_str(account, "enterpriseId").or_else(|| get_str(account, "enterprise_id")) {
        headers.insert("X-Enterprise-Id".to_string(), eid.clone());
        headers.insert("X-Tenant-Id".to_string(), eid);
    }
    if let Some(domain) = get_str(account, "domain") {
        headers.insert("X-Domain".to_string(), domain);
    }
    headers
}

/// 对话专属出站头族（对齐官方桌面客户端形态）。
pub fn chat_headers(account: &Value, conversation_id: &str, request_id: &str) -> HashMap<String, String> {
    let variant = variant_of(account);
    let uid = get_str(account, "uid").unwrap_or_default();
    let token = get_str(account, "access_token").unwrap_or_default();
    let mut headers = common_headers(account);
    headers.insert("Authorization".to_string(), format!("Bearer {token}"));
    headers.insert(
        "Accept".to_string(),
        "application/json, text/event-stream".to_string(),
    );
    headers.insert("User-Agent".to_string(), client_ua(variant));
    headers.insert("X-Domain".to_string(), "chat".to_string());
    headers.insert("X-Product".to_string(), "SaaS".to_string());
    headers.insert("X-IDE-Name".to_string(), "WorkBuddy".to_string());
    headers.insert("X-IDE-Type".to_string(), "WorkBuddy".to_string());
    headers.insert("X-IDE-Version".to_string(), DESKTOP_VERSION.to_string());
    headers.insert("x-codebuddy-request".to_string(), request_id.to_string());
    headers.insert("X-Conversation-Id".to_string(), conversation_id.to_string());
    headers.insert("X-User-Id".to_string(), uid);
    headers
}

// ---------------------------------------------------------------------------
// 入站请求体改写
// ---------------------------------------------------------------------------

/// 把 OpenAI 入站请求体改写成上游能吃的形态（返回副本，不改调用方数据）。
pub fn prepare_body(payload: &Value) -> Result<Value, GatewayError> {
    let Some(obj) = payload.as_object() else {
        return Err(GatewayError::invalid_request("请求体必须是 JSON 对象"));
    };
    let mut body = obj.clone();

    let messages_ok = body
        .get("messages")
        .and_then(|v| v.as_array())
        .map(|arr| !arr.is_empty())
        .unwrap_or(false);
    if !messages_ok {
        return Err(GatewayError::invalid_request("messages 不能为空"));
    }

    normalize_roles(&mut body);
    normalize_tool_choice(&mut body);
    normalize_reasoning_effort(&mut body);
    translate_max_tokens(&mut body);

    // stream_options.include_usage：官方流默认不带 usage，客户端要就显式打开。
    if body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false) {
        let include = body
            .get("stream_options")
            .and_then(|v| v.get("include_usage"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        body.insert("stream_options".to_string(), json!({"include_usage": include}));
    }
    body.remove("stream_options_if_absent");

    if body.get("max_tokens").is_none() && body.get("max_completion_tokens").is_none() {
        body.insert("max_tokens".to_string(), json!(DEFAULT_MAX_TOKENS));
    }
    Ok(Value::Object(body))
}

/// `max_completion_tokens` 是较新的字段名，上游只认 `max_tokens`。
fn translate_max_tokens(body: &mut serde_json::Map<String, Value>) {
    let modern = body.remove("max_completion_tokens");
    if let Some(value) = modern {
        if !body.contains_key("max_tokens") {
            body.insert("max_tokens".to_string(), value);
        }
    }
    if let Some(limit) = body.get("max_tokens").and_then(|v| v.as_i64()) {
        if limit <= 0 {
            body.insert("max_tokens".to_string(), json!(DEFAULT_MAX_TOKENS));
        }
    }
}

/// 系统角色归一：部分客户端发 `developer`，上游只认 `system`。
fn normalize_roles(body: &mut serde_json::Map<String, Value>) {
    let Some(messages) = body.get_mut("messages").and_then(|v| v.as_array_mut()) else {
        return;
    };
    for message in messages.iter_mut() {
        if let Some(obj) = message.as_object_mut() {
            let role = obj.get("role").and_then(|v| v.as_str()).unwrap_or("");
            if role == "developer" {
                obj.insert("role".to_string(), json!("system"));
            }
        }
    }
}

/// `tool_choice` 归一：`required` / 对象形态转成上游认识的写法。
fn normalize_tool_choice(body: &mut serde_json::Map<String, Value>) {
    let Some(choice) = body.get("tool_choice").cloned() else {
        return;
    };
    match choice {
        Value::String(text) if text == "required" => {
            body.insert("tool_choice".to_string(), json!("auto"));
        }
        Value::Null => {
            body.remove("tool_choice");
        }
        _ => {}
    }
}

/// 推理档位归一：不在白名单内的一律去掉，避免上游直接报错。
fn normalize_reasoning_effort(body: &mut serde_json::Map<String, Value>) {
    let Some(effort) = body.get("reasoning_effort").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
        return;
    };
    if !REASONING_EFFORTS.contains(&effort.as_str()) {
        body.remove("reasoning_effort");
    }
}

// ---------------------------------------------------------------------------
// 上游调用
// ---------------------------------------------------------------------------

/// 打开上游流（这里一次性读回全文，交由 SSE 解析层切帧）。
pub async fn open_chat(
    account: &Value,
    body: &Value,
    timeout_note: &mut Option<String>,
    conversation_id: &str,
    request_id: &str,
) -> Result<(String, Vec<String>), GatewayError> {
    let variant = variant_of(account);
    let (chat_base, _, _) = bases(variant);
    let headers = chat_headers(account, conversation_id, request_id);
    let payload = serde_json::to_string(body).unwrap_or_else(|_| "{}".to_string());

    let mut last_error: Option<GatewayError> = None;
    for path in chat_paths(variant) {
        let url = format!("{chat_base}{path}");
        let body_value: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
        let (status, _, text) =
            http_request_raw(&url, "POST", Some(body_value), Some(&headers), None, true).await;
        if status == 404 || status == 405 {
            continue;
        }
        if !(200..300).contains(&status) {
            let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let err = map_upstream_error(status, &parsed);
            *timeout_note = Some(format!("对话请求 HTTP {status}：{}", err.message));
            last_error = Some(err);
            continue;
        }
        return Ok((text, chat_paths(variant).iter().map(|s| s.to_string()).collect()));
    }

    Err(last_error.unwrap_or_else(|| {
        GatewayError::new(
            502,
            "upstream_error",
            "对话请求全部候选路径都失败",
            "upstream_error",
        )
    }))
}

/// 逐行切上游 SSE，返回每个 `data:` 之后的 payload（含 `[DONE]`）。
pub fn iter_raw_frames(raw: &str) -> Vec<String> {
    let mut frames = Vec::new();
    for line in raw.split('\n') {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("data:") {
            let payload = rest.trim();
            if !payload.is_empty() {
                frames.push(payload.to_string());
                if frames.len() >= MAX_UPSTREAM_FRAMES {
                    break;
                }
            }
        }
    }
    frames
}

/// 剥上游噪声，重建规范 chunk。`first_id` 用于同流 id 一致。
pub fn normalize_frame(frame: &Value, first_id: &str) -> Value {
    let delta_in = frame
        .get("delta")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let mut delta = serde_json::Map::new();
    if let Some(content) = delta_in
        .get("content")
        .or_else(|| frame.get("content"))
        .and_then(|v| v.as_str())
    {
        if !content.is_empty() {
            delta.insert("content".to_string(), json!(content));
        }
    }
    if let Some(reasoning) = delta_in
        .get("reasoning_content")
        .or_else(|| frame.get("reasoning_content"))
        .and_then(|v| v.as_str())
    {
        if !reasoning.is_empty() {
            delta.insert("reasoning_content".to_string(), json!(reasoning));
        }
    }
    if let Some(role) = delta_in
        .get("role")
        .or_else(|| frame.get("role"))
        .and_then(|v| v.as_str())
    {
        delta.insert("role".to_string(), json!(role));
    }

    let tool_calls = delta_in
        .get("tool_calls")
        .cloned()
        .or_else(|| frame.get("tool_calls").cloned());
    if let Some(calls) = tool_calls {
        if calls.is_array() {
            delta.insert("tool_calls".to_string(), calls);
        }
    } else if let Some(legacy) = delta_in
        .get("function_call")
        .cloned()
        .or_else(|| frame.get("function_call").cloned())
    {
        // 旧式 function_call 折算成 tool_calls[0]，上游两种形态都出现过。
        let name = legacy
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let arguments = legacy
            .get("arguments")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        delta.insert(
            "tool_calls".to_string(),
            json!([{
                "index": 0,
                "type": "function",
                "function": {"name": name, "arguments": arguments}
            }]),
        );
    }

    let finish_reason = frame
        .get("finish_reason")
        .cloned()
        .unwrap_or(Value::Null);

    json!({
        "id": first_id,
        "object": "chat.completion.chunk",
        "created": 0,
        "model": frame.get("model").cloned().unwrap_or(Value::Null),
        "choices": [{
            "index": 0,
            "delta": Value::Object(delta),
            "finish_reason": finish_reason,
        }],
    })
}

/// 合并流式 tool_calls 分片（同 index 的 arguments 拼接）。
pub fn merge_tool_call_delta(merged: &mut serde_json::Map<String, Value>, delta: &Value) {
    let Some(incoming) = delta.get("tool_calls").and_then(|v| v.as_array()) else {
        return;
    };
    let entry = merged
        .entry("tool_calls".to_string())
        .or_insert_with(|| json!([]));
    let Some(list) = entry.as_array_mut() else {
        return;
    };
    for call in incoming {
        let index = call.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
        let target = list
            .iter_mut()
            .find(|item| item.get("index").and_then(|v| v.as_i64()).unwrap_or(0) == index);
        match target {
            Some(existing) => {
                let name = call
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let args = call
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if let Some(function) = existing.get_mut("function").and_then(|f| f.as_object_mut()) {
                    if !name.is_empty() {
                        function.insert("name".to_string(), json!(name));
                    }
                    if !args.is_empty() {
                        let prev = function
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        function.insert("arguments".to_string(), json!(format!("{prev}{args}")));
                    }
                }
            }
            None => {
                let mut item = call.clone();
                if let Some(obj) = item.as_object_mut() {
                    obj.insert("index".to_string(), json!(index));
                }
                list.push(item);
            }
        }
    }
}

/// 把上游 SSE 聚合成单个 OpenAI `chat.completion`（对外非流式）。
pub fn aggregate(frames: &[String], model_hint: &str) -> Result<Value, GatewayError> {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut finish_reason = "stop".to_string();
    let mut merged: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut usage: Option<Value> = None;
    let mut seen = false;

    for frame in frames {
        if frame == "[DONE]" {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(frame) else {
            continue;
        };
        seen = true;
        if let Some(text) = obj.get("content").and_then(|v| v.as_str()) {
            content.push_str(text);
        }
        if let Some(delta) = obj.get("delta") {
            if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                content.push_str(text);
            }
            if let Some(text) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                reasoning.push_str(text);
            }
            merge_tool_call_delta(&mut merged, delta);
        }
        if let Some(reason) = obj.get("finish_reason").and_then(|v| v.as_str()) {
            finish_reason = reason.to_string();
        }
        if let Some(u) = obj.get("usage") {
            if !u.is_null() {
                usage = Some(u.clone());
            }
        }
    }

    if !seen {
        return Err(GatewayError::new(
            502,
            "upstream_error",
            "上游返回了空流（无任何有效数据事件）",
            "upstream_error",
        ));
    }

    let mut message = serde_json::Map::new();
    message.insert("role".to_string(), json!("assistant"));
    message.insert("content".to_string(), json!(content));
    if !reasoning.is_empty() {
        message.insert("reasoning_content".to_string(), json!(reasoning));
    }
    if let Some(calls) = merged.get("tool_calls") {
        message.insert("tool_calls".to_string(), calls.clone());
    }

    Ok(json!({
        "id": format!("chatcmpl-{}", new_hex32()),
        "object": "chat.completion",
        "created": chrono::Local::now().timestamp(),
        "model": model_hint,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish_reason,
        }],
        "usage": usage.unwrap_or_else(|| json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0})),
    }))
}

/// 把上游流回译成 OpenAI SSE 字符串序列（含收尾 `[DONE]`）。
pub fn iter_openai_sse(raw: &str, model_hint: &str) -> Vec<String> {
    let first_id = format!("chatcmpl-wbm{}", new_hex32());
    let mut out = Vec::new();
    for frame in iter_raw_frames(raw) {
        if frame == "[DONE]" {
            break;
        }
        let Ok(obj) = serde_json::from_str::<Value>(&frame) else {
            continue;
        };
        let mut chunk = normalize_frame(&obj, &first_id);
        if let Some(obj_mut) = chunk.as_object_mut() {
            obj_mut.insert("model".to_string(), json!(model_hint));
            obj_mut.insert("created".to_string(), json!(chrono::Local::now().timestamp()));
        }
        out.push(format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap_or_default()));
    }
    out.push("data: [DONE]\n\n".to_string());
    out
}

// ---------------------------------------------------------------------------
// 对界面暴露的快照
// ---------------------------------------------------------------------------

/// 某个档位的完整快照（界面用）。
pub fn snapshot(variant: WbVariant) -> Value {
    let cfg = realm_config(variant);
    let usage = load_usage();
    let node = usage.get(realm_str(variant)).cloned().unwrap_or_else(empty_usage);
    let today = today_key();
    let today_used = if node.get("date").and_then(|v| v.as_str()).unwrap_or("") == today {
        node.get("today").and_then(|v| v.as_i64()).unwrap_or(0)
    } else {
        0
    };
    let candidates = candidates(variant);
    let ids: Vec<String> = candidates
        .iter()
        .filter_map(|acc| get_str(acc, "id"))
        .collect();

    json!({
        "realm": realm_str(variant),
        "label": realm_label(variant),
        "enabled": cfg.enabled,
        "key": cfg.key,
        // 只给路径：具体 host:port 由宿主决定（桌面端与 webui 的端口不同），
        // 前端用 location.origin 拼完整地址，避免写死端口后两边对不上。
        "path": match variant {
            WbVariant::Cn => "/v1",
            WbVariant::Ai => "/v1-global",
        },
        "dailyLimit": cfg.daily_limit,
        "today": today_used,
        "total": node.get("total").and_then(|v| v.as_i64()).unwrap_or(0),
        "tokens": node.get("tokens").and_then(|v| v.as_i64()).unwrap_or(0),
        "failures": node.get("failures").and_then(|v| v.as_i64()).unwrap_or(0),
        "pool": cfg.pool,
        "accountCount": candidates.len(),
        "cooling": pool().cooling_count(&ids),
    })
}

/// 两个档位的快照（界面默认要的那一份）。
pub fn payload() -> Value {
    json!({
        "gatewayVersion": GATEWAY_VERSION,
        "cn": snapshot(WbVariant::Cn),
        "global": snapshot(WbVariant::Ai),
    })
}

// ---------------------------------------------------------------------------
// 单测
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realm_normalization() {
        assert_eq!(realm_str(gw_realm(Some("global"))), "global");
        assert_eq!(realm_str(gw_realm(Some(" GLOBAL "))), "global");
        assert_eq!(realm_str(gw_realm(Some(""))), "cn");
        assert_eq!(realm_str(gw_realm(None)), "cn");
    }

    #[test]
    fn stable_id_is_deterministic_and_scoped() {
        let a = stable_id("u1", "machine");
        assert_eq!(a.len(), 36);
        assert_eq!(a, stable_id("u1", "machine"));
        assert_ne!(a, stable_id("u1", "session"));
        assert_ne!(a, stable_id("u2", "machine"));
    }

    #[test]
    fn auth_check_accepts_bearer_and_raw() {
        let key = "sk-wb-test";
        assert!(check_auth(Some("Bearer sk-wb-test"), key));
        assert!(check_auth(Some("sk-wb-test"), key));
        assert!(!check_auth(Some("Bearer sk-wb-other"), key));
        assert!(!check_auth(None, key));
    }

    #[test]
    fn prepare_body_rejects_empty_messages() {
        assert!(prepare_body(&json!({"messages": []})).is_err());
        assert!(prepare_body(&json!("nope")).is_err());
    }

    #[test]
    fn prepare_body_translates_and_defaults() {
        let body = prepare_body(&json!({
            "messages": [{"role": "developer", "content": "hi"}],
            "max_completion_tokens": 256,
            "reasoning_effort": "bogus"
        }))
        .unwrap();
        assert_eq!(body["max_tokens"], json!(256));
        assert!(body.get("max_completion_tokens").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["messages"][0]["role"], json!("system"));

        let defaulted = prepare_body(&json!({"messages": [{"role": "user", "content": "x"}]})).unwrap();
        assert_eq!(defaulted["max_tokens"], json!(DEFAULT_MAX_TOKENS));
    }

    #[test]
    fn upstream_risk_code_maps_to_risk_error() {
        let err = map_upstream_error(403, &json!({"code": 11140, "displayMsg": "request illegal"}));
        assert!(err.code.starts_with("upstream_risk_control"));
        assert_eq!(err.status, 502);
    }

    #[test]
    fn frames_split_and_normalize() {
        let raw = "data: {\"delta\":{\"content\":\"你\"}}\n\ndata: {\"delta\":{\"content\":\"好\"}}\n\ndata: [DONE]\n\n";
        let frames = iter_raw_frames(raw);
        assert_eq!(frames.len(), 3);
        let chunk = normalize_frame(&json!({"delta": {"content": "你"}}), "chatcmpl-x");
        assert_eq!(chunk["object"], json!("chat.completion.chunk"));
        assert_eq!(chunk["choices"][0]["delta"]["content"], json!("你"));
    }

    #[test]
    fn aggregate_builds_non_stream_response() {
        let frames = vec![
            "{\"delta\":{\"content\":\"he\"}}".to_string(),
            "{\"delta\":{\"content\":\"llo\"}}".to_string(),
            "[DONE]".to_string(),
        ];
        let out = aggregate(&frames, "auto").unwrap();
        assert_eq!(out["object"], json!("chat.completion"));
        assert_eq!(out["choices"][0]["message"]["content"], json!("hello"));
    }

    #[test]
    fn aggregate_rejects_empty_stream() {
        let frames = vec!["[DONE]".to_string()];
        assert!(aggregate(&frames, "auto").is_err());
    }

    #[test]
    fn tool_call_delta_merges_arguments() {
        let mut merged = serde_json::Map::new();
        merge_tool_call_delta(
            &mut merged,
            &json!({"tool_calls": [{"index": 0, "function": {"name": "f", "arguments": "{\"a\""}}]}),
        );
        merge_tool_call_delta(
            &mut merged,
            &json!({"tool_calls": [{"index": 0, "function": {"arguments": ":1}"}}]}),
        );
        assert_eq!(merged["tool_calls"][0]["function"]["name"], json!("f"));
        assert_eq!(merged["tool_calls"][0]["function"]["arguments"], json!("{\"a\":1}"));
    }

    #[test]
    fn pool_prefers_least_recently_used() {
        let pool = Pool::default();
        let ids = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert!(pool.acquire("a"));
        pool.release("a");
        let order = pool.order(&ids);
        assert_eq!(order[0], "b");
    }

    #[test]
    fn pool_puts_cooling_accounts_last() {
        let pool = Pool::default();
        let ids = vec!["a".to_string(), "b".to_string()];
        pool.acquire("a");
        pool.release("a");
        pool.note_error("b");
        let order = pool.order(&ids);
        assert_eq!(order[0], "a");
        assert_eq!(order[1], "b");
        assert!(!pool.acquire("b"));
    }

    #[test]
    fn models_payload_filters_items_without_id() {
        let payload = models_payload(&[
            json!({"id": "glm-5.3-flash", "vendor": "zhipu", "reasoning": true, "supportedEfforts": ["low"]}),
            json!({"vendor": "x"}),
        ]);
        assert_eq!(payload["data"].as_array().unwrap().len(), 1);
        assert_eq!(payload["data"][0]["id"], json!("glm-5.3-flash"));
        assert_eq!(payload["data"][0]["supported_efforts"], json!(["low"]));
    }
}
