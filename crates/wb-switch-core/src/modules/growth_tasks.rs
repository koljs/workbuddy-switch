//! 成长中心任务读取与真实达成奖励领取。
//!
//! 只按服务端回传的真实进度展示任务；批量领取只处理 `current >= target` 且
//! 尚未领取的项目。这里不生成或上报伪造的对话、桌面、分享、专家等行为事件。

use super::account::{build_auth_headers, variant_of};
use super::config::http_request;
use super::gateway::bases;
use super::variant::WbVariant;
use serde_json::{json, Value};
use std::collections::HashMap;
use uuid::Uuid;

pub const WEB_BASE_CN: &str = "https://www.workbuddy.cn";
pub const WEB_BASE_GLOBAL: &str = "https://www.workbuddy.ai";
pub const TASKS_LIST_PATH: &str = "/v2/activity/growth/tasks";
pub const TASKS_ACCEPT_PATH: &str = "/v2/activity/growth/tasks/accept";
pub const GIFT_PATH: &str = "/billing/meter/claim-gift";
pub const COMPENSATION_PATH: &str = "/billing/meter/claim-compensation";
pub const STREAK_PATH: &str = "/activity/growth/streak";
pub const REDEEM_PATH: &str = "/activity/growth/redeem";
pub const LOTTERY_SUMMARY_PATH: &str = "/activity/growth/lottery/summary";
pub const LOTTERY_DRAW_PATH: &str = "/activity/growth/lottery/draw";
pub const HEATMAP_PATH: &str = "/activity/growth/heatmap";
pub const MAKEUP_USE_PATH: &str = "/activity/growth/makeup-cards/use";
pub const SCHOOL_BASE: &str = "/portal/activity/school";

pub fn int_from(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0),
        Some(Value::Bool(b)) => i64::from(*b),
        _ => 0,
    }
}

pub fn web_base(account: &Value) -> &'static str {
    match variant_of(account) {
        WbVariant::Ai => WEB_BASE_GLOBAL,
        WbVariant::Cn => WEB_BASE_CN,
    }
}

fn is_transport_error(resp: &Value) -> bool {
    resp.get("code").and_then(Value::as_i64) == Some(-1)
}

async fn growth_request(account: &Value, method: &str, path: &str, body: Option<Value>) -> Value {
    // 成长任务 / streak / lottery 走 chat 域（国内版 copilot.tencent.com），
    // 不能拿签到用的 billing endpoint 替代。
    let (chat_base, _, _) = bases(variant_of(account));
    let url = format!("{chat_base}{path}");
    let headers = build_auth_headers(account);
    http_request(&url, method, body, Some(&headers)).await
}

async fn billing_request(account: &Value, method: &str, path: &str, body: Option<Value>) -> Value {
    let (_, billing_base, _) = bases(variant_of(account));
    let url = format!("{billing_base}{path}");
    let headers = build_auth_headers(account);
    http_request(&url, method, body, Some(&headers)).await
}

fn web_headers(account: &Value) -> HashMap<String, String> {
    let base = web_base(account);
    let mut headers = HashMap::new();
    headers.insert(
        "Authorization".to_string(),
        format!(
            "Bearer {}",
            account
                .get("access_token")
                .and_then(Value::as_str)
                .unwrap_or("")
        ),
    );
    headers.insert(
        "Accept".to_string(),
        "application/json, text/plain, */*".to_string(),
    );
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers.insert("Origin".to_string(), base.to_string());
    headers.insert(
        "Referer".to_string(),
        format!("{base}/profile/growth-center"),
    );
    headers.insert("x-client-platform".to_string(), "web".to_string());
    if let Some(uid) = account
        .get("uid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        headers.insert("X-User-Id".to_string(), uid.to_string());
    }
    if let Some(eid) = account
        .get("enterprise_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        headers.insert("X-Enterprise-Id".to_string(), eid.to_string());
        headers.insert("X-Tenant-Id".to_string(), eid.to_string());
    }
    headers
}

async fn web_request(account: &Value, method: &str, path: &str, body: Option<Value>) -> Value {
    let url = format!("{}{path}", web_base(account));
    let headers = web_headers(account);
    http_request(&url, method, body, Some(&headers)).await
}

/// 公共只读 growth/billing 接口（福利状态与次数使用）。
pub async fn growth_get_public(account: &Value, path: &str) -> Value {
    growth_request(account, "GET", path, None).await
}

/// 公共写入 growth 接口。
pub async fn growth_post_public(account: &Value, path: &str, body: Option<Value>) -> Value {
    growth_request(account, "POST", path, body).await
}

/// 公共只读 billing 接口。
pub async fn billing_get_public(account: &Value, path: &str) -> Value {
    billing_request(account, "GET", path, None).await
}

/// 公共写入 billing 接口（仅真实福利 / 已发放资格使用）。
pub async fn billing_post_public(account: &Value, path: &str, body: Option<Value>) -> Value {
    billing_request(account, "POST", path, body).await
}

/// 公共只读 web 域接口（仅网页活动页面的数据使用）。
pub async fn web_get_public(account: &Value, path: &str) -> Value {
    web_request(account, "GET", path, None).await
}

/// 公共写入 web 域接口（领奖端点使用）。
pub async fn web_post_public(account: &Value, path: &str, body: Option<Value>) -> Value {
    web_request(account, "POST", path, body).await
}

/// 将上游任务记录归一化，优先采用 `progress` 子对象。
pub fn normalize_task(item: &Value) -> Value {
    let mut current = int_from(item.get("current"));
    let mut target = int_from(item.get("target"));
    if let Some(progress) = item.get("progress").filter(|v| v.is_object()) {
        let child_current = int_from(progress.get("current"));
        let child_target = int_from(progress.get("target"));
        if child_target > 0 || child_current > 0 {
            current = child_current;
            target = child_target;
        }
    }
    let accept_status = item
        .get("accept_status")
        .and_then(Value::as_str)
        .unwrap_or("");
    let claimed = accept_status == "claimed";
    let code = item
        .get("task_code")
        .or_else(|| item.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("");
    json!({
        "task_code": code,
        "title": item.get("title").and_then(Value::as_str).unwrap_or(""),
        "description": item.get("description").and_then(Value::as_str).unwrap_or(""),
        "task_desc": item.get("task_desc").and_then(Value::as_str).unwrap_or(""),
        "credit": int_from(item.get("reward_credit")),
        "energy": int_from(item.get("reward_energy")),
        "task_type": item.get("task_type").and_then(Value::as_str).unwrap_or(""),
        "tag": item.get("tag").and_then(Value::as_str).unwrap_or(""),
        "locked": item.get("locked").and_then(Value::as_bool).unwrap_or(false),
        "target": target,
        "current": current,
        "accept_status": accept_status,
        "status": item.get("status").and_then(Value::as_str).unwrap_or(""),
        "claimable": !claimed && !item.get("locked").and_then(Value::as_bool).unwrap_or(false) && target > 0 && current >= target,
        "claimed": claimed,
    })
}

/// 读取国内版成长任务；国际版不提供这套积分成长中心。
pub async fn list_tasks(account: &Value) -> Value {
    if variant_of(account) != WbVariant::Cn {
        return json!({
            "ok": false,
            "unsupported": true,
            "message": "国际版成长任务体系不同，暂不支持该任务列表",
            "tasks": [],
        });
    }
    let data = growth_request(account, "GET", TASKS_LIST_PATH, None).await;
    if is_transport_error(&data) {
        return json!({
            "ok": false,
            "message": data.get("message").and_then(Value::as_str).unwrap_or("读取成长任务失败"),
            "tasks": [],
        });
    }
    let tasks: Vec<Value> = data
        .get("tasks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|task| task.is_object())
        .map(normalize_task)
        .collect();
    json!({ "ok": true, "tasks": tasks })
}

fn safe_task_code(code: &str) -> bool {
    !code.is_empty()
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// 领取任务奖励。调用方必须先确认 `claimable == true`。
pub async fn claim_reward(account: &Value, task_code: &str) -> Value {
    if !safe_task_code(task_code) {
        return json!({ "ok": false, "message": "任务编号格式无效" });
    }
    let path = format!("/activity/growth/tasks/{task_code}/claim");
    let data = web_request(account, "POST", &path, None).await;
    if is_transport_error(&data) {
        return json!({
            "ok": false,
            "message": data.get("message").and_then(Value::as_str).unwrap_or("领奖请求失败"),
        });
    }
    if data.get("already_claimed").and_then(Value::as_bool) == Some(true) {
        return json!({ "ok": true, "already_claimed": true, "credit": 0, "energy": 0 });
    }
    let code = data.get("code").and_then(Value::as_i64);
    if code.is_some_and(|value| value != 0 && value != 200) {
        return json!({
            "ok": false,
            "message": data.get("message").or_else(|| data.get("msg"))
                .and_then(Value::as_str).unwrap_or("上游拒绝领取"),
        });
    }
    json!({
        "ok": true,
        "credit": int_from(data.get("credit")),
        "energy": int_from(data.get("energy")),
    })
}

/// 领取所有当前真实达成、未锁定且未领取的任务（不会产生任务行为事件）。
pub async fn claim_ready_tasks(account: &Value) -> Value {
    let listing = list_tasks(account).await;
    if listing.get("ok").and_then(Value::as_bool) != Some(true) {
        return json!({
            "ok": false,
            "unsupported": listing.get("unsupported").and_then(Value::as_bool).unwrap_or(false),
            "message": listing.get("message").and_then(Value::as_str).unwrap_or("读取成长任务失败"),
            "results": [],
            "credit": 0,
            "energy": 0,
        });
    }
    let tasks = listing
        .get("tasks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut results = Vec::new();
    let mut credit = 0i64;
    let mut energy = 0i64;
    for task in tasks
        .iter()
        .filter(|task| task.get("claimable").and_then(Value::as_bool) == Some(true))
    {
        let task_code = task.get("task_code").and_then(Value::as_str).unwrap_or("");
        let claimed = claim_reward(account, task_code).await;
        let ok = claimed.get("ok").and_then(Value::as_bool) == Some(true);
        let task_credit = int_from(claimed.get("credit"));
        let task_energy = int_from(claimed.get("energy"));
        credit += task_credit;
        energy += task_energy;
        results.push(json!({
            "task_code": task_code,
            "title": task.get("title").and_then(Value::as_str).unwrap_or(""),
            "ok": ok,
            "already_claimed": claimed.get("already_claimed").and_then(Value::as_bool).unwrap_or(false),
            "credit": task_credit,
            "energy": task_energy,
            "message": claimed.get("message").and_then(Value::as_str).unwrap_or(if ok { "奖励已领取" } else { "领取失败" }),
        }));
    }
    json!({ "ok": true, "results": results, "credit": credit, "energy": energy })
}

/// 领取新手礼包，返回到账积分。
pub async fn claim_gift(account: &Value) -> i64 {
    let resp = billing_request(account, "POST", GIFT_PATH, Some(json!({}))).await;
    int_from(resp.get("credit"))
}

/// 领取活动补偿，返回到账积分。
pub async fn claim_compensation(account: &Value) -> i64 {
    let resp = billing_request(account, "POST", COMPENSATION_PATH, Some(json!({}))).await;
    int_from(resp.get("credit"))
}

/// 连登状态原始响应。
pub async fn growth_streak_full(account: &Value) -> Value {
    growth_request(account, "GET", STREAK_PATH, None).await
}

/// 读取补签卡热力图。
pub async fn heatmap(account: &Value) -> Value {
    growth_request(account, "GET", HEATMAP_PATH, None).await
}

/// 使用一张真实存在的补签卡。
pub async fn use_makeup_card(account: &Value, target_date: &str) -> Value {
    growth_request(
        account,
        "POST",
        MAKEUP_USE_PATH,
        Some(json!({ "target_date": target_date })),
    )
    .await
}

/// 兑换连登指定档位。
pub async fn redeem_tier(account: &Value, tier: &str) -> Value {
    growth_request(
        account,
        "POST",
        REDEEM_PATH,
        Some(json!({ "tier": tier, "client_token": Uuid::new_v4().to_string() })),
    )
    .await
}

/// 剩余抽奖次数。
pub async fn lottery_chances(account: &Value) -> i64 {
    let response = growth_request(account, "GET", LOTTERY_SUMMARY_PATH, None).await;
    int_from(response.get("chances"))
}

/// 抽一次奖（只消耗服务端真实已发放的次数）。
pub async fn lottery_draw(account: &Value) -> Value {
    growth_request(
        account,
        "POST",
        LOTTERY_DRAW_PATH,
        Some(json!({ "client_token": Uuid::new_v4().to_string() })),
    )
    .await
}

/// 开学季接口前缀 wrapper（福利模块调用）。
pub async fn school_request_public(
    account: &Value,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Value {
    billing_request(account, method, &format!("{SCHOOL_BASE}{path}"), body).await
}

/// 开学季抽奖：每次调用携带新 draw_uuid，消耗一个真实可用次数。
pub async fn school_draw(account: &Value) -> Value {
    school_request_public(
        account,
        "POST",
        "/wheel/draw",
        Some(json!({ "draw_uuid": Uuid::new_v4().to_string() })),
    )
    .await
}

/// 检查是否为上游传输错误。
pub fn is_transport_error_response(resp: &Value) -> bool {
    is_transport_error(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_task_prefers_progress_and_only_claims_real_completion() {
        let task = normalize_task(&json!({
            "task_code": "chat_5",
            "current": 99,
            "target": 99,
            "accept_status": "accepted",
            "progress": { "current": 2, "target": 5 },
            "reward_credit": "30"
        }));
        assert_eq!(task["current"], 2);
        assert_eq!(task["target"], 5);
        assert_eq!(task["claimable"], false);
        assert_eq!(task["credit"], 30);
    }

    #[test]
    fn normalize_task_marks_only_unlocked_completed_rewards_claimable() {
        assert_eq!(
            normalize_task(&json!({"code":"x","current":5,"target":5,"accept_status":"accepted"}))
                ["claimable"],
            true
        );
        assert_eq!(
            normalize_task(&json!({"code":"x","current":5,"target":5,"accept_status":"claimed"}))
                ["claimable"],
            false
        );
        assert_eq!(
            normalize_task(
                &json!({"code":"x","current":5,"target":5,"accept_status":"accepted","locked":true})
            )["claimable"],
            false
        );
    }

    #[test]
    fn task_code_guard_rejects_path_separators() {
        assert!(safe_task_code("Model_chat_GLM5.2"));
        assert!(!safe_task_code("../claim"));
        assert!(!safe_task_code("x/y"));
        assert!(!safe_task_code(""));
    }

    #[test]
    fn integer_parser_handles_strings_and_missing_values() {
        assert_eq!(int_from(Some(&json!("17"))), 17);
        assert_eq!(int_from(Some(&json!("invalid"))), 0);
        assert_eq!(int_from(None), 0);
    }

    #[test]
    fn web_domain_switches_by_variant() {
        assert_eq!(web_base(&json!({})), WEB_BASE_CN);
        assert_eq!(web_base(&json!({"variant":"ai"})), WEB_BASE_GLOBAL);
    }
}
