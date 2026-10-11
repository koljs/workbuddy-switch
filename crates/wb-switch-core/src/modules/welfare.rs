//! 每日福利入口。
//!
//! 八步固定顺序：**签到 → 新手礼包 → 活动补偿 → 猫猫旅行 → 连登兑换 → 抽奖 →
//! 补签卡 → 开学季**。只调用真实福利端点、使用服务端已发放的抽奖/补签次数；
//! 不模拟分享、页面浏览或对话等用户行为。每步统一返回 `{ok, credit, message}`，
//! 某一步失败不影响后续独立福利步骤。

use super::account::variant_of;
use super::checkin::checkin_account;
use super::growth_tasks::{
    claim_compensation, claim_gift, growth_streak_full, heatmap, int_from, lottery_chances,
    lottery_draw, redeem_tier, school_draw, school_request_public, use_makeup_card,
};
use super::travel::depart_travel_for_account;
use super::variant::WbVariant;
use serde_json::{json, Value};

/// 福利步骤标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WelfareKind {
    /// 每日签到（复用既有签到链路）。
    Checkin,
    /// 新手礼包。
    Gift,
    /// 活动补偿。
    Compensation,
    /// 猫猫旅行（复用既有旅行链路）。
    Travel,
    /// 连登兑换。
    Redeem,
    /// 抽奖。
    Lottery,
    /// 补签卡。
    Makeup,
    /// 开学季活动。
    School,
}

impl WelfareKind {
    /// 步骤名（UI 与 `only` 过滤都用它）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Checkin => "签到",
            Self::Gift => "新手礼包",
            Self::Compensation => "活动补偿",
            Self::Travel => "猫猫旅行",
            Self::Redeem => "连登兑换",
            Self::Lottery => "抽奖",
            Self::Makeup => "补签卡",
            Self::School => "开学季",
        }
    }
}

/// 福利步骤表，顺序即执行顺序。
pub const WELFARE_STEPS: [WelfareKind; 8] = [
    WelfareKind::Checkin,
    WelfareKind::Gift,
    WelfareKind::Compensation,
    WelfareKind::Travel,
    WelfareKind::Redeem,
    WelfareKind::Lottery,
    WelfareKind::Makeup,
    WelfareKind::School,
];

/// 单步结果。`credit` 为本次到账积分（没有积分概念的步骤为 0）。
pub fn outcome(ok: bool, credit: i64, message: &str) -> Value {
    json!({ "ok": ok, "credit": credit, "message": message })
}

/// 按 `only` 过滤步骤；`only` 为空表示全部执行。
pub fn filter_steps(only: Option<&[String]>) -> Vec<WelfareKind> {
    match only {
        None => WELFARE_STEPS.to_vec(),
        Some(list) => WELFARE_STEPS
            .iter()
            .copied()
            .filter(|kind| list.iter().any(|label| label == kind.as_str()))
            .collect(),
    }
}

/// 汇总一次福利执行：成功步数、总积分、失败步骤名。
pub fn welfare_summary(results: &[Value]) -> Value {
    let ok_count = results
        .iter()
        .filter(|r| r.get("ok").and_then(Value::as_bool) == Some(true))
        .count();
    let credit: i64 = results
        .iter()
        .map(|r| int_from(r.get("credit")))
        .sum::<i64>();
    let failed: Vec<String> = results
        .iter()
        .filter(|r| r.get("ok").and_then(Value::as_bool) != Some(true))
        .map(|r| {
            r.get("step")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        })
        .collect();
    json!({
        "total": results.len(),
        "ok": ok_count,
        "credit": credit,
        "failed": failed,
    })
}

/// 开学季任务只领取服务端明确可领或实际进度已达标、未锁定的项目。
pub fn school_task_is_claimable(task: &Value) -> bool {
    let code = task
        .get("task_code")
        .or_else(|| task.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let safe_code = !code.is_empty()
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    if !safe_code
        || task.get("claimed").and_then(Value::as_bool) == Some(true)
        || task.get("status").and_then(Value::as_str) == Some("claimed")
        || task.get("locked").and_then(Value::as_bool) == Some(true)
    {
        return false;
    }
    if task.get("claimable").and_then(Value::as_bool) == Some(true)
        || task.get("status").and_then(Value::as_str) == Some("claimable")
    {
        return true;
    }
    let current = int_from(
        task.get("current")
            .or_else(|| task.get("progress").and_then(|p| p.get("current"))),
    );
    let target = int_from(
        task.get("target")
            .or_else(|| task.get("progress").and_then(|p| p.get("target"))),
    );
    target > 0 && current >= target
}

/// 某天是否被漏签（补签卡步骤用）。`cells` 中 `score=0` 表示漏签；未知结构保守返回 `false`。
pub fn heatmap_yesterday_missed(heatmap: &Value, yesterday: &str) -> bool {
    // 官方热力图以 cells 数组表示每日状态：score=0 才是未签到。
    // 缺少目标日期或结构无法识别时返回 false，避免误用补签卡。
    let cells = heatmap
        .get("cells")
        .or_else(|| heatmap.get("data").and_then(|d| d.get("cells")))
        .and_then(Value::as_array);
    let Some(cells) = cells else { return false };
    cells.iter().any(|cell| {
        let date = cell.get("date").and_then(Value::as_str).unwrap_or("");
        date.get(..10) == Some(yesterday) && int_from(cell.get("score")) == 0
    })
}

// ---------------------------------------------------------------------------
// 各步骤
// ---------------------------------------------------------------------------

/// 签到：复用既有签到链路，`success` / `already` 都算成功。
pub async fn step_checkin(account: &Value) -> Value {
    let resp = checkin_account(account).await;
    let result = resp.get("result").and_then(Value::as_str).unwrap_or("");
    match result {
        "success" => outcome(true, 0, "签到成功"),
        "already" => outcome(true, 0, "今天已经签过到了"),
        "skipped" => outcome(false, 0, "该档位不支持签到"),
        "inactive" => outcome(false, 0, "签到活动未开启或已过期"),
        _ => outcome(
            false,
            0,
            &format!(
                "签到失败：{}",
                resp.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("未知原因")
            ),
        ),
    }
}

/// 新手礼包：领到积分才算成功。
pub async fn step_gift(account: &Value) -> Value {
    let credit = claim_gift(account).await;
    if credit > 0 {
        outcome(true, credit, &format!("新手礼包 +{credit} 分"))
    } else {
        outcome(false, 0, "新手礼包已领过")
    }
}

/// 活动补偿：没有补偿是正常态（不算失败，但也不该报成功）。
pub async fn step_compensation(account: &Value) -> Value {
    let credit = claim_compensation(account).await;
    if credit > 0 {
        outcome(true, credit, &format!("活动补偿 +{credit} 分"))
    } else {
        outcome(false, 0, "暂无活动补偿")
    }
}

/// 猫猫旅行：出发或领取，任一命中即算成功。
pub async fn step_travel(account: &Value) -> Value {
    let resp = depart_travel_for_account(account).await;
    let skip = resp.get("skip").and_then(Value::as_str).unwrap_or("");
    if !skip.is_empty() {
        return outcome(false, 0, &format!("猫猫旅行跳过：{skip}"));
    }
    let credit = int_from(resp.get("rewardCredit").or_else(|| resp.get("credit")));
    if resp.get("ok").and_then(Value::as_bool) == Some(true) {
        let message = resp
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("已处理");
        outcome(true, credit, message)
    } else {
        outcome(
            false,
            0,
            &format!(
                "猫猫旅行失败：{}",
                resp.get("message")
                    .or_else(|| resp.get("error"))
                    .and_then(Value::as_str)
                    .unwrap_or("未知原因")
            ),
        )
    }
}

/// 连登兑换：只请求已达天数且服务端未标记已领/锁定的档位。
pub async fn step_redeem(account: &Value) -> Value {
    let full = growth_streak_full(account).await;
    let streak = full
        .get("streak")
        .filter(|v| v.is_object())
        .unwrap_or(&full);
    let days = int_from(streak.get("days"));
    let redemption = full
        .get("redemption_status")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut redeemed = Vec::new();
    let mut failed = Vec::new();
    for (tier, threshold) in [("7d", 7), ("14d", 14), ("28d", 28)] {
        if days < threshold {
            continue;
        }
        let status = redemption
            .get(format!("tier_{tier}_status"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        if ["claimed", "redeemed", "locked"].contains(&status.as_str()) {
            continue;
        }
        let response = redeem_tier(account, tier).await;
        let code = response.get("code").and_then(Value::as_i64);
        if super::growth_tasks::is_transport_error_response(&response)
            || code.is_some_and(|value| value != 0 && value != 200)
        {
            failed.push(tier.to_string());
        } else {
            redeemed.push(tier.to_string());
        }
    }
    if !redeemed.is_empty() {
        outcome(
            true,
            0,
            &format!("连登 {days} 天，已兑换 {} 档", redeemed.join("/")),
        )
    } else if !failed.is_empty() {
        outcome(false, 0, &format!("连登兑换请求失败：{}", failed.join("/")))
    } else {
        outcome(false, 0, &format!("连登 {days} 天，暂无可兑换档位"))
    }
}

/// 抽奖：有多少次抽多少次（上限 20 次，防止接口被限流时无限重试）。
pub async fn step_lottery(account: &Value) -> Value {
    let chances = lottery_chances(account).await;
    if chances <= 0 {
        return outcome(false, 0, "没有抽奖次数");
    }
    let rounds = chances.min(20);
    let mut prizes: Vec<String> = Vec::new();
    let mut credit = 0i64;
    for _ in 0..rounds {
        let data = lottery_draw(account).await;
        let code = data.get("code").and_then(Value::as_i64);
        if is_upstream(&data) || code.is_some_and(|value| value != 0 && value != 200) {
            break;
        }
        credit += int_from(data.get("credit").or_else(|| data.get("credit_amount")));
        let name = data
            .get("prize_name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("谢谢参与");
        prizes.push(name.to_string());
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
    if prizes.is_empty() {
        return outcome(false, 0, "抽奖中断");
    }
    outcome(
        true,
        credit,
        &format!("抽奖 {} 次：{}", prizes.len(), prizes.join("、")),
    )
}

/// 补签卡：只在昨天确实漏签、且手里有卡时才用。
pub async fn step_makeup(account: &Value) -> Value {
    let heatmap_data = heatmap(account).await;
    let yesterday = yesterday_str();
    if !heatmap_yesterday_missed(&heatmap_data, &yesterday) {
        return outcome(false, 0, "昨日未漏签");
    }
    let full = growth_streak_full(account).await;
    let balance = int_from(full.get("makeup_cards").and_then(|c| c.get("balance")));
    if balance <= 0 {
        return outcome(false, 0, "昨日漏签但无补签卡");
    }
    let resp = use_makeup_card(account, &yesterday).await;
    if is_upstream(&resp) {
        return outcome(false, 0, "补签失败：网络错误");
    }
    outcome(true, 0, &format!("已用补签卡补上 {yesterday}"))
}

/// 开学季：只领取服务端已标记可领取或进度已达标的任务奖励；不模拟浏览/分享。
pub async fn step_school(account: &Value) -> Value {
    let tasks = school_request_public(account, "GET", "/tasks", None).await;
    if is_upstream(&tasks) {
        return outcome(false, 0, "开学季活动读取失败");
    }
    if tasks.get("in_period").and_then(Value::as_bool) == Some(false) {
        return outcome(false, 0, "开学季活动未在周期内");
    }
    let mut credit = 0i64;
    let mut task_claims = 0usize;
    let list = tasks
        .get("tasks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for task in list.iter() {
        let code = match task
            .get("task_code")
            .or_else(|| task.get("code"))
            .and_then(Value::as_str)
        {
            Some(code) => code,
            None => continue,
        };
        if !school_task_is_claimable(task) {
            continue;
        }
        let claimed =
            school_request_public(account, "POST", &format!("/tasks/{code}/claim"), None).await;
        let response_code = claimed.get("code").and_then(Value::as_i64);
        if !is_upstream(&claimed) && !response_code.is_some_and(|value| value != 0 && value != 200)
        {
            task_claims += 1;
            credit += int_from(
                claimed
                    .get("credit")
                    .or_else(|| claimed.get("chance_granted")),
            );
        }
    }

    let config = school_request_public(account, "GET", "/config", None).await;
    let chances = int_from(config.get("chance").and_then(|c| c.get("balance")));
    let mut prizes: Vec<String> = Vec::new();
    for _ in 0..chances.min(20) {
        let draw = school_draw(account).await;
        let response_code = draw.get("code").and_then(Value::as_i64);
        if is_upstream(&draw) || response_code.is_some_and(|value| value != 0 && value != 200) {
            break;
        }
        let prize = draw.get("prize_code").and_then(Value::as_str).unwrap_or("");
        let draw_credit = int_from(draw.get("credit_amount"));
        credit += draw_credit;
        prizes.push(if draw_credit > 0 {
            format!("{prize} +{draw_credit}分")
        } else {
            prize.to_string()
        });
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
    let vouchers = school_request_public(account, "GET", "/vouchers", None).await;
    let voucher_count = vouchers
        .get("items")
        .and_then(Value::as_array)
        .map(|items| items.len())
        .unwrap_or(0);
    if task_claims == 0 && prizes.is_empty() && voucher_count == 0 {
        return outcome(false, 0, "开学季暂无可领内容");
    }
    let prize_summary = if prizes.is_empty() {
        "无".to_string()
    } else {
        prizes.join("、")
    };
    outcome(
        true,
        credit,
        &format!(
            "开学季：领取任务 {task_claims} 项；抽奖 {} 次（{}）；券 {voucher_count} 张",
            prizes.len(),
            prize_summary
        ),
    )
}

/// 国际版每日活跃应由用户实际对话完成；不自动发起占位对话。
pub async fn step_intl_daily(account: &Value) -> Value {
    if variant_of(account) != WbVariant::Ai {
        return outcome(false, 0, "仅国际版需要每日活跃");
    }
    outcome(
        false,
        0,
        "国际版请在 WorkBuddy AI 中完成真实对话；此处不会代发占位消息",
    )
}

/// 顺序执行福利步骤，返回每步结果（含 `step` 名）。
pub async fn run_welfare(account: &Value, only: Option<&[String]>) -> Vec<Value> {
    if variant_of(account) == WbVariant::Ai {
        let item = step_intl_daily(account).await;
        let mut item = item;
        if let Some(map) = item.as_object_mut() {
            map.insert("step".to_string(), json!("网页打卡(每日活跃)"));
        }
        return vec![item];
    }
    let mut results = Vec::new();
    for kind in filter_steps(only) {
        let outcome_value = match kind {
            WelfareKind::Checkin => step_checkin(account).await,
            WelfareKind::Gift => step_gift(account).await,
            WelfareKind::Compensation => step_compensation(account).await,
            WelfareKind::Travel => step_travel(account).await,
            WelfareKind::Redeem => step_redeem(account).await,
            WelfareKind::Lottery => step_lottery(account).await,
            WelfareKind::Makeup => step_makeup(account).await,
            WelfareKind::School => step_school(account).await,
        };
        let mut item = outcome_value;
        if let Some(map) = item.as_object_mut() {
            map.insert("step".to_string(), json!(kind.as_str()));
        }
        results.push(item);
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
    results
}

fn is_upstream(resp: &Value) -> bool {
    resp.get("code").and_then(Value::as_i64) == Some(-1)
}

fn yesterday_str() -> String {
    // 用本地日历日期回退一天，避免夏令时切换日直接减 24 小时造成日期偏差。
    let today = chrono::Local::now().date_naive();
    today
        .pred_opt()
        .unwrap_or(today)
        .format("%Y-%m-%d")
        .to_string()
}

/// 国际版每日活跃不自动创建占位会话，避免代替用户生成伪活动记录。
pub async fn intl_web_daily(account: &Value) -> Value {
    if variant_of(account) == WbVariant::Ai {
        outcome(
            false,
            0,
            "请在 WorkBuddy AI 中完成真实对话；此处不会代发占位消息",
        )
    } else {
        outcome(false, 0, "该操作仅适用于国际版账号")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_labels_and_order_are_stable() {
        let labels: Vec<&str> = WELFARE_STEPS.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "签到",
                "新手礼包",
                "活动补偿",
                "猫猫旅行",
                "连登兑换",
                "抽奖",
                "补签卡",
                "开学季"
            ]
        );
        assert_eq!(WELFARE_STEPS.len(), 8);
    }

    #[test]
    fn filter_steps_respects_only_list() {
        assert_eq!(filter_steps(None).len(), 8);
        let only = vec!["抽奖".to_string(), "签到".to_string()];
        let picked = filter_steps(Some(&only));
        // 结果按表内顺序，不受 only 的顺序影响
        assert_eq!(picked, vec![WelfareKind::Checkin, WelfareKind::Lottery]);
        assert!(filter_steps(Some(&["不存在".to_string()])).is_empty());
    }

    #[test]
    fn outcome_shape_is_fixed() {
        let v = outcome(true, 120, "新手礼包 +120 分");
        assert_eq!(v["ok"], true);
        assert_eq!(v["credit"], 120);
        assert_eq!(v["message"], "新手礼包 +120 分");
    }

    #[test]
    fn summary_counts_ok_and_credit() {
        let results = vec![
            json!({"step": "签到", "ok": true, "credit": 0}),
            json!({"step": "新手礼包", "ok": true, "credit": 300}),
            json!({"step": "抽奖", "ok": false, "credit": 0}),
        ];
        let s = welfare_summary(&results);
        assert_eq!(s["total"], 3);
        assert_eq!(s["ok"], 2);
        assert_eq!(s["credit"], 300);
        assert_eq!(s["failed"], json!(["抽奖"]));
    }

    #[test]
    fn missed_day_detection_is_conservative() {
        let heatmap = json!({"cells": [
            {"date": "2026-10-10T00:00:00Z", "score": 0},
            {"date": "2026-10-11T00:00:00Z", "score": 1},
        ]});
        assert!(heatmap_yesterday_missed(&heatmap, "2026-10-10"));
        assert!(!heatmap_yesterday_missed(&heatmap, "2026-10-11"));
        assert!(!heatmap_yesterday_missed(&heatmap, "2026-10-09"));
        // 认不出结构时不补签，避免浪费补签卡
        assert!(!heatmap_yesterday_missed(&json!({}), "2026-10-10"));
    }

    #[test]
    fn missed_day_accepts_nested_cells() {
        let heatmap = json!({"data": {"cells": [{"date": "2026-10-10T00:00:00Z", "score": 0}]}});
        assert!(heatmap_yesterday_missed(&heatmap, "2026-10-10"));
    }

    #[test]
    fn upstream_detection_uses_code_minus_one() {
        assert!(is_upstream(&json!({"code": -1})));
        assert!(!is_upstream(&json!({"code": 0})));
        assert!(!is_upstream(&json!({})));
    }

    #[test]
    fn growth_endpoint_paths_match_upstream() {
        assert_eq!(
            crate::modules::growth_tasks::STREAK_PATH,
            "/activity/growth/streak"
        );
        assert_eq!(
            crate::modules::growth_tasks::HEATMAP_PATH,
            "/activity/growth/heatmap"
        );
        assert_eq!(
            crate::modules::growth_tasks::MAKEUP_USE_PATH,
            "/activity/growth/makeup-cards/use"
        );
    }

    #[test]
    fn school_claim_only_accepts_real_completed_unlocked_tasks() {
        assert!(school_task_is_claimable(
            &json!({"code":"daily","current":3,"target":3})
        ));
        assert!(school_task_is_claimable(
            &json!({"code":"manual-complete","claimable":true})
        ));
        assert!(!school_task_is_claimable(
            &json!({"code":"daily","current":0,"target":3})
        ));
        assert!(!school_task_is_claimable(
            &json!({"code":"daily","current":3,"target":3,"locked":true})
        ));
        assert!(!school_task_is_claimable(
            &json!({"code":"share","claimable":false,"target":1,"current":0})
        ));
        assert!(!school_task_is_claimable(
            &json!({"code":"../claim","claimable":true})
        ));
    }
}
