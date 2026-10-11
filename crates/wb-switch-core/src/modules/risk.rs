//! 风控体检：给每个账号真发一条最小对话，把结论写回账号库。
//!
//! 上游（腾讯侧）的风控是**按账号**下手的：被风控的号 token 照样能续期、积分照样
//! 能查，但一发对话就被拒（`code 11140` / "request illegal"）。所以只有真发一条
//! 对话，才知道这个号还能不能用。换 IP、换 token 都无效——限制挂在号本身。
//!
//! 四种结论，处置方式完全不同：
//! - `ok`      对话链路正常，这个号现在能用
//! - `risk`    上游 11140，只能等官方解除或在官方客户端内反馈申诉
//! - `auth`    登录态失效，重新登录这个号即可
//! - `network` 网络问题（不一定是号坏了）
//! - `other`   凭证文件缺失等，按提示那行文字排查
//!
//! 体检**只读不改号**：不把号打进冷却、不算进网关用量。唯一的写入是把结论记进
//! 账号记录的 `risk_state` / `risk_at`，供网关挑号时参考（见
//! [`is_risk_blocked`]）。

use chrono::Local;
use serde_json::{json, Value};

use crate::modules::account::{
    account_display_name, envelope_token_error, get_str, load_accounts, upsert_account, variant_of,
};
use crate::modules::gateway::{
    aggregate, open_chat, realm_str, GatewayError, UPSTREAM_RISK_CONTROL_CODE,
};
use crate::modules::refresh::ensure_fresh_token;
use crate::modules::variant::WbVariant;

/// 被判风控后的"保鲜期"（秒）：这段时间内网关不派活给它。
///
/// 6 小时是折中：短了会把注定 403 的死号反复派出去，长了会拖慢风控解除后的恢复。
/// 跑一次体检就能立刻更新这个结论。
pub const RISK_FRESH_SECONDS: i64 = 6 * 3600;

/// 体检结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskState {
    Ok,
    Risk,
    Auth,
    Network,
    Other,
}

impl RiskState {
    pub fn as_str(self) -> &'static str {
        match self {
            RiskState::Ok => "ok",
            RiskState::Risk => "risk",
            RiskState::Auth => "auth",
            RiskState::Network => "network",
            RiskState::Other => "other",
        }
    }

    /// 界面上的小标签。
    pub fn label(self) -> &'static str {
        match self {
            RiskState::Ok => "✅ 可用",
            RiskState::Risk => "🚫 风控拦截",
            RiskState::Auth => "🔑 登录态失效",
            RiskState::Network => "⚠️ 网络问题",
            RiskState::Other => "⚠️ 其他",
        }
    }
}

/// 把一次对话尝试的结果归类成体检结论 → (state, 上游 code, 给人看的话)。
pub fn classify(error: Option<&GatewayError>) -> (RiskState, Option<i64>, String) {
    let Some(error) = error else {
        return (RiskState::Ok, None, "对话链路正常，这个号现在能用".to_string());
    };

    // 网络层失败：status 0 表示请求根本没发出去 / 没收到响应。
    if error.status == 0 {
        return (
            RiskState::Network,
            None,
            format!("网络问题（可能是本机网络，不一定是号坏了）：{}", error.message),
        );
    }

    if let Some(code) = parse_risk_code(&error.code) {
        if code == UPSTREAM_RISK_CONTROL_CODE {
            return (
                RiskState::Risk,
                Some(code),
                format!(
                    "上游风控拦截（code {code}）——该号已无法对话；换 IP / 换 token 均无效，\
                     需在官方客户端内反馈申诉，或等官方自行解除：{}",
                    error.message
                ),
            );
        }
        return (
            RiskState::Risk,
            Some(code),
            format!("上游风控拦截（code {code}）：{}", error.message),
        );
    }

    if error.status == 401 || error.status == 403 {
        return (
            RiskState::Auth,
            None,
            format!("登录态失效，重新登录该号即可：{}", error.message),
        );
    }

    (
        RiskState::Other,
        None,
        format!("其他问题：{}", error.message),
    )
}

/// 从 `upstream_risk_control:11140` 这类 code 里取出数字。
fn parse_risk_code(code: &str) -> Option<i64> {
    let tail = code.rsplit(':').next()?;
    tail.trim().parse().ok()
}

/// 解析 `risk_at`（本地时间字符串）到毫秒时间戳。
fn parse_risk_at(raw: &str) -> Option<i64> {
    let naive = chrono::NaiveDateTime::parse_from_str(raw.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    let local = Local.from_local_datetime(&naive).single()?;
    Some(local.timestamp_millis())
}

use chrono::TimeZone;

/// 这个号是不是「刚被体检判为风控」——是的话先别让它进网关轮转池。
///
/// 只认 `risk`（上游 11140）。`auth` / `network` 不进这个判断：前者要用户重新登录，
/// 后者可能只是本机断网，都不该把号从池子里摘掉。
pub fn is_risk_blocked(account: &Value) -> bool {
    if get_str(account, "risk_state").as_deref() != Some("risk") {
        return false;
    }
    let Some(raw) = get_str(account, "risk_at") else {
        return false;
    };
    let Some(stamp) = parse_risk_at(&raw) else {
        return false;
    };
    let now = Local::now().timestamp_millis();
    now - stamp < RISK_FRESH_SECONDS * 1000
}

/// 体检一个账号，返回一行结论。单号失败绝不抛。
pub async fn check_one(account: &Value) -> Value {
    let id = get_str(account, "id").unwrap_or_default();
    let name = account_display_name(account);
    let uid = get_str(account, "uid").unwrap_or_default();
    let variant = variant_of(account);

    // 信封 token（WorkBuddy 5.6+ 本机加密）解不出明文，发请求必然 401，
    // 直接给一条可读结论，别让用户对着 "无效 token" 猜。
    if let Some(hint) = envelope_token_error(account) {
        return row(&id, &name, &uid, variant, RiskState::Other, None, &hint);
    }

    let cfg = crate::modules::config::load_checkin_config();
    let working = ensure_fresh_token(account.clone(), &cfg).await;

    let body = json!({
        "messages": [{"role": "user", "content": "1"}],
        "max_tokens": 16,
        "stream": true,
    });

    let conversation_id = crate::modules::gateway::new_hex32();
    let request_id = crate::modules::gateway::new_hex32();
    let mut note: Option<String> = None;

    let outcome = match open_chat(&working, &body, &mut note, &conversation_id, &request_id).await {
        Ok((raw, _)) => {
            let frames = crate::modules::gateway::iter_raw_frames(&raw);
            match aggregate(&frames, "auto") {
                Ok(_) => None,
                Err(err) => Some(err),
            }
        }
        Err(err) => Some(err),
    };

    let (state, code, message) = classify(outcome.as_ref());
    persist_risk(&working, &id, state);
    row(&id, &name, &uid, variant, state, code, &message)
}

/// 体检结论写回账号记录（网关挑号时会读它）。
fn persist_risk(account: &Value, account_id: &str, state: RiskState) {
    let mut next = account.clone();
    if let Some(obj) = next.as_object_mut() {
        obj.insert("risk_state".to_string(), json!(state.as_str()));
        obj.insert(
            "risk_at".to_string(),
            json!(Local::now().format("%Y-%m-%d %H:%M:%S").to_string()),
        );
    }
    // 记录 id 缺失时补上，避免 upsert 走 uid 回退路径时丢身份。
    if get_str(&next, "id").is_none() && !account_id.is_empty() {
        next["id"] = json!(account_id);
    }
    let _ = upsert_account(&next);
}

fn row(
    id: &str,
    name: &str,
    uid: &str,
    variant: WbVariant,
    state: RiskState,
    code: Option<i64>,
    message: &str,
) -> Value {
    json!({
        "id": id,
        "name": name,
        "uid": uid,
        "realm": realm_str(variant),
        "state": state.as_str(),
        "label": state.label(),
        "upstreamCode": code,
        "message": message,
        "checkedAt": Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
    })
}

/// 体检一批账号。`realm` 为空 = 两个档位都跑。
pub async fn check_all(realm: Option<&str>) -> Value {
    let target = realm
        .map(|raw| raw.trim().to_lowercase())
        .filter(|raw| !raw.is_empty());
    let accounts: Vec<Value> = load_accounts()
        .into_iter()
        .filter(|acc| match &target {
            Some(raw) if raw == "all" => true,
            Some(raw) => realm_str(variant_of(acc)) == raw,
            None => true,
        })
        .collect();

    let mut rows = Vec::new();
    let mut counts = json!({"ok": 0, "risk": 0, "auth": 0, "network": 0, "other": 0});
    for account in &accounts {
        let result = check_one(account).await;
        let state = result
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("other")
            .to_string();
        if let Some(node) = counts.get_mut(&state).and_then(|v| v.as_i64().map(|n| n + 1)) {
            counts[&state] = json!(node);
        }
        rows.push(result);
    }

    json!({
        "total": rows.len(),
        "counts": counts,
        "rows": rows,
        "checkedAt": Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_ok_when_no_error() {
        let (state, code, _) = classify(None);
        assert_eq!(state, RiskState::Ok);
        assert!(code.is_none());
    }

    #[test]
    fn classify_risk_for_11140() {
        let err = GatewayError::new(
            502,
            &format!("upstream_risk_control:{UPSTREAM_RISK_CONTROL_CODE}"),
            "request illegal",
            "upstream_error",
        );
        let (state, code, message) = classify(Some(&err));
        assert_eq!(state, RiskState::Risk);
        assert_eq!(code, Some(11140));
        assert!(message.contains("11140"));
    }

    #[test]
    fn classify_auth_for_401() {
        let err = GatewayError::new(401, "upstream_error", "unauthorized", "authentication_error");
        assert_eq!(classify(Some(&err)).0, RiskState::Auth);
    }

    #[test]
    fn classify_network_for_status_zero() {
        let err = GatewayError::new(0, "upstream_error", "connection refused", "upstream_error");
        assert_eq!(classify(Some(&err)).0, RiskState::Network);
    }

    #[test]
    fn risk_block_window_expires() {
        let fresh = json!({
            "risk_state": "risk",
            "risk_at": Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        });
        assert!(is_risk_blocked(&fresh));

        let stale = json!({"risk_state": "risk", "risk_at": "2020-01-01 00:00:00"});
        assert!(!is_risk_blocked(&stale));

        // 非 risk 结论不参与剔除。
        let auth = json!({
            "risk_state": "auth",
            "risk_at": Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        });
        assert!(!is_risk_blocked(&auth));
    }
}
