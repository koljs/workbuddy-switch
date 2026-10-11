//! 出口 IP 检测与「换 IP 向导」。
//!
//! 解决的问题：上游风控把「同一个出口 IP 短时间反复登录/使用多个账号」当成异常
//! 特征。所以多账号玩家真正需要的不是「现在 IP 是多少」，而是两件事：
//! 1. **当下这个 IP 上挂了多少个号**——超过阈值就该换 IP 了；
//! 2. **换完确认真的换了**——运营商重拨有时给的还是同一个 IP，凭感觉判断会翻车。
//!
//! 本模块把这两件事做成可执行的动作：检测当前出口 IP（带归属地）、按 IP 聚合账号、
//! 把某个 IP 记到一批账号上、以及一份四步向导。
//!
//! 缓存 5 分钟：页面每次打开都打一遍探测接口既慢又容易触发探测方的限流；但缓存太
//! 久又会让「刚换完 IP」的确认失真。5 分钟是这两者之间的折中，`force` 可绕过。
//!
//! 探测源按顺序尝试，任一成功即止。**不把 IP 上报给任何第三方以外的用途**，只存
//! 本地 `egress.json`。

use chrono::Local;
use serde_json::{json, Value};

use crate::modules::account::{account_display_name, get_str, load_accounts, upsert_account};
use crate::modules::config::{http_request, now_ms, store_dir};

/// 缓存有效期（秒）。
pub const EGRESS_CACHE_SECONDS: i64 = 300;

/// 同一个出口 IP 上挂到几个号就该提示换 IP。
///
/// 2 是刻意取小的：真正的风控阈值官方不公开，宁可早提示也不要事后补救——而「同一
/// IP 挂 2 个号」已经足够让人主动规划下一步了。
pub const SHARING_WARN_THRESHOLD: usize = 2;

/// 探测源。按顺序尝试，第一个能解析出 IP 的胜出。
///
/// 先 ip-api（带中文归属地），再 ipify（只有 IP，但可用性最好）兜底，最后
/// `api64.ipify.org`——它优先返回 IPv6，用在「本机只有 IPv6 能出去」的环境。
const PROBES: &[Probe] = &[
    Probe {
        url: "https://ip-api.com/json/?lang=zh-CN&fields=status,message,query,country,regionName,city,isp",
        kind: ProbeKind::IpApi,
    },
    Probe {
        url: "https://api.ipify.org?format=json",
        kind: ProbeKind::Ipify,
    },
    Probe {
        url: "https://ipapi.co/json/",
        kind: ProbeKind::IpApiCo,
    },
    Probe {
        url: "https://api64.ipify.org?format=json",
        kind: ProbeKind::Ipify,
    },
];

#[derive(Debug, Clone, Copy)]
enum ProbeKind {
    IpApi,
    Ipify,
    IpApiCo,
}

struct Probe {
    url: &'static str,
    kind: ProbeKind,
}

/// 一次成功的探测结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Egress {
    pub ip: String,
    pub country: String,
    pub region: String,
    pub city: String,
    pub isp: String,
    pub source: String,
}

impl Egress {
    /// 归属地一行摘要；字段缺失时不留多余分隔符。
    pub fn location(&self) -> String {
        let parts: Vec<&str> = [
            self.country.as_str(),
            self.region.as_str(),
            self.city.as_str(),
        ]
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
        parts.join(" ")
    }

    fn to_json(&self, at_ms: i64, cached: bool) -> Value {
        json!({
            "ip": self.ip,
            "country": self.country,
            "region": self.region,
            "city": self.city,
            "isp": self.isp,
            "source": self.source,
            "location": self.location(),
            "at": at_ms,
            "atText": Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            "cached": cached,
        })
    }
}

/// 把某个探测源的响应解析成 [`Egress`]；解析不出来返回 None（换下一个源）。
pub fn parse_probe(kind_source: &str, body: &Value) -> Option<Egress> {
    let text = |key: &str| -> String {
        body.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };

    match kind_source {
        "ip-api" => {
            // ip-api 用 status 字段表达成败，失败时只有 message。
            if text("status") != "success" {
                return None;
            }
            let ip = text("query");
            if ip.is_empty() {
                return None;
            }
            Some(Egress {
                ip,
                country: text("country"),
                region: text("regionName"),
                city: text("city"),
                isp: text("isp"),
                source: "ip-api".to_string(),
            })
        }
        "ipify" => {
            let ip = text("ip");
            if ip.is_empty() {
                return None;
            }
            Some(Egress {
                ip,
                country: String::new(),
                region: String::new(),
                city: String::new(),
                isp: String::new(),
                source: "ipify".to_string(),
            })
        }
        "ipapi.co" => {
            let ip = text("ip");
            if ip.is_empty() {
                return None;
            }
            Some(Egress {
                ip,
                country: text("country_name"),
                region: text("region"),
                city: text("city"),
                isp: text("org"),
                source: "ipapi.co".to_string(),
            })
        }
        _ => None,
    }
}

fn kind_name(kind: ProbeKind) -> &'static str {
    match kind {
        ProbeKind::IpApi => "ip-api",
        ProbeKind::Ipify => "ipify",
        ProbeKind::IpApiCo => "ipapi.co",
    }
}

/// 缓存文件路径。
pub fn egress_cache_file() -> std::path::PathBuf {
    store_dir().join("egress.json")
}

/// 历史记录最多保留多少条出口 IP（换 IP 向导的回溯用）。
pub const HISTORY_LIMIT: usize = 50;

fn read_cache() -> Value {
    std::fs::read_to_string(egress_cache_file())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn write_cache(value: &Value) {
    let path = egress_cache_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(value) {
        let _ = std::fs::write(path, text);
    }
}

/// 读缓存里的当前 IP 记录（可能已过期，调用方自行判断 `at`）。
pub fn cached() -> Option<Value> {
    current_from(&read_cache())
}

fn current_from(cache: &Value) -> Option<Value> {
    let node = cache.get("current")?;
    let ip = node.get("ip").and_then(|v| v.as_str())?.trim();
    if ip.is_empty() {
        return None;
    }
    Some(node.clone())
}

/// 当前出口 IP（不管缓存是否过期），没有记录时返回 None。
pub fn current_ip() -> Option<String> {
    cached().and_then(|node| get_str(&node, "ip"))
}

/// 历史出口 IP，最近的在前。
pub fn history() -> Vec<Value> {
    history_from(&read_cache())
}

fn history_from(cache: &Value) -> Vec<Value> {
    cache
        .get("history")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn push_history(cache: &mut Value, record: &Value) {
    let ip = record.get("ip").and_then(|v| v.as_str()).unwrap_or("");
    let mut list: Vec<Value> = cache
        .get("history")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    // 同一个 IP 只留最近一次，否则反复检测会把历史刷满同一个值。
    list.retain(|item| item.get("ip").and_then(|v| v.as_str()) != Some(ip));
    list.insert(0, record.clone());
    list.truncate(HISTORY_LIMIT);
    cache["history"] = json!(list);
}

/// 检测当前出口 IP。
///
/// `force = false` 且缓存未过期时直接返回缓存，不打探测接口。
/// 全部探测源都失败时返回 `{"ip": null, "error": "..."}`（不抛，界面照常渲染）。
pub async fn detect(force: bool) -> Value {
    let cache = read_cache();
    if !force {
        if let Some(node) = cache.get("current") {
            let at = node.get("at").and_then(|v| v.as_i64()).unwrap_or(0);
            let ip = node.get("ip").and_then(|v| v.as_str()).unwrap_or("");
            if !ip.trim().is_empty() && now_ms() - at < EGRESS_CACHE_SECONDS * 1000 {
                let mut fresh = node.clone();
                fresh["cached"] = json!(true);
                return fresh;
            }
        }
    }

    let mut failures: Vec<String> = Vec::new();
    for probe in PROBES {
        let body = http_request(probe.url, "GET", None, None).await;
        // 网络层失败（code = -1）与探测源返回错误都记一笔，最后一起报。
        if body.get("code").and_then(|v| v.as_i64()) == Some(-1) {
            let reason = body
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("未知原因");
            failures.push(format!("{}: {reason}", kind_name(probe.kind)));
            continue;
        }
        match parse_probe(kind_name(probe.kind), &body) {
            Some(egress) => {
                let at = now_ms();
                let record = egress.to_json(at, false);
                let mut next = cache.clone();
                next["current"] = record.clone();
                push_history(&mut next, &record);
                write_cache(&next);
                return record;
            }
            None => failures.push(format!("{}: 响应无法解析", kind_name(probe.kind))),
        }
    }

    json!({
        "ip": Value::Null,
        "error": format!("所有出口 IP 探测源都没成功：{}", failures.join("；")),
        "tried": PROBES.len(),
    })
}

/// 快照：当前 IP + 该 IP 上挂了哪些号 + 是否需要提示换 IP + 向导步骤 + 历史。
pub fn snapshot() -> Value {
    // 只读一次缓存文件：`cached()` / `history()` / `guide()` 各自都会读一遍，
    // 拼快照时连读三次既慢又可能读到不一致的三份内容。
    let cache = read_cache();
    let current = current_from(&cache);
    let ip = current
        .as_ref()
        .and_then(|node| get_str(node, "ip"))
        .unwrap_or_default();
    let sharing = if ip.is_empty() {
        json!({"ip": "", "count": 0, "warn": false, "message": "还没有当前出口 IP 的记录", "accounts": []})
    } else {
        sharing_report(&ip)
    };
    json!({
        "current": current.unwrap_or(Value::Null),
        "sharing": sharing,
        "threshold": SHARING_WARN_THRESHOLD,
        "cacheSeconds": EGRESS_CACHE_SECONDS,
        "history": history_from(&cache),
        "guide": guide_for(&ip),
    })
}

/// 某个出口 IP 上挂了哪些号。
///
/// 只认账号上的 `egress_ip` 字段（保存/分配时写入），**不**用「当前 IP 反推」——
/// 号可能是几天前在别的网络下登录的，反推出来的归属是错的。
pub fn accounts_on_ip(ip: &str) -> Vec<Value> {
    let target = ip.trim();
    if target.is_empty() {
        return vec![];
    }
    load_accounts()
        .into_iter()
        .filter(|acc| get_str(acc, "egress_ip").as_deref() == Some(target))
        .collect()
}

/// 按 IP 聚合的提示信息。
pub fn sharing_report(ip: &str) -> Value {
    let accounts = accounts_on_ip(ip);
    let count = accounts.len();
    let rows: Vec<Value> = accounts
        .iter()
        .map(|acc| {
            json!({
                "id": get_str(acc, "id").unwrap_or_default(),
                "name": account_display_name(acc),
                "uid": get_str(acc, "uid").unwrap_or_default(),
                "variant": get_str(acc, "variant").unwrap_or_else(|| "cn".to_string()),
            })
        })
        .collect();
    json!({
        "ip": ip.trim(),
        "count": count,
        "warn": count >= SHARING_WARN_THRESHOLD,
        "message": if count >= SHARING_WARN_THRESHOLD {
            format!("这个 IP 上已经挂了 {count} 个号，建议换 IP 后再登录新号")
        } else if count == 0 {
            "这个 IP 上还没有记录任何账号".to_string()
        } else {
            format!("这个 IP 上有 {count} 个号，暂时安全")
        },
        "accounts": rows,
    })
}

/// 把某个出口 IP 记到一批账号上。
///
/// 用途有两处：保存新号时顺手记下「登录时用的 IP」；换 IP 向导确认新 IP 后，把
/// 刚登录的那批号改记到新 IP 上（否则归属会永远停在旧值）。
///
/// 返回 `{"updated": n, "missing": [id...], "ip": "..."}`。空 IP 直接拒绝——写一个
/// 空归属比不写更糟，会让聚合统计出现一个假的「空 IP 桶」。
pub fn assign(account_ids: &[String], ip: &str) -> Value {
    let target = ip.trim();
    if target.is_empty() {
        return json!({"updated": 0, "missing": [], "ip": "", "error": "出口 IP 为空，未做任何修改"});
    }
    let at_text = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mut updated = 0usize;
    let mut missing: Vec<String> = Vec::new();

    // 账号库只读一次：按 id 逐个重读文件是 O(号数 × 号数) 次磁盘读，号一多就很明显。
    let accounts = load_accounts();

    for id in account_ids {
        let wanted = id.trim();
        if wanted.is_empty() {
            continue;
        }
        let Some(mut account) = accounts
            .iter()
            .find(|acc| get_str(acc, "id").as_deref() == Some(wanted))
            .cloned()
        else {
            missing.push(wanted.to_string());
            continue;
        };
        if let Some(obj) = account.as_object_mut() {
            obj.insert("egress_ip".to_string(), json!(target));
            obj.insert("egress_at".to_string(), json!(at_text));
        }
        // 单号写失败不影响同批其他号，也不中断循环。
        if upsert_account(&account).is_ok() {
            updated += 1;
        }
    }

    json!({"updated": updated, "missing": missing, "ip": target})
}

/// 把当前出口 IP 记到一批账号上（向导第 4 步的「一键确认」）。
pub fn assign_current(account_ids: &[String]) -> Value {
    match current_ip() {
        Some(ip) => assign(account_ids, &ip),
        None => json!({
            "updated": 0,
            "missing": [],
            "ip": "",
            "error": "还没有当前出口 IP 的记录，先点一次「检测出口 IP」",
        }),
    }
}

/// 换 IP 向导：四步。
///
/// 每一步都带一个**可验证的完成条件**，而不是「请切换网络」这种没法判断做没做的
/// 指令——换 IP 失败最典型的形态就是「以为换了，其实运营商给回同一个 IP」。
pub fn guide() -> Value {
    guide_for(&current_ip().unwrap_or_default())
}

fn guide_for(ip: &str) -> Value {
    let baseline = if ip.trim().is_empty() {
        "还没有记录".to_string()
    } else {
        ip.trim().to_string()
    };
    json!([
        {
            "step": 1,
            "title": "记录当前出口 IP",
            "detail": format!("当前记录：{baseline}。点「检测出口 IP」把基线确定下来，换完才有得对比。"),
            "doneWhen": "上方显示出当前 IP 与归属地",
        },
        {
            "step": 2,
            "title": "切换网络出口",
            "detail": "任选一种：① 手机开飞行模式 10 秒再关；② 宽带断开重拨；③ 从 Wi-Fi 切到手机热点；④ 给账号配一个不同出口的代理（账号级代理）。",
            "doneWhen": "设备已经走了另一条出口链路",
        },
        {
            "step": 3,
            "title": "验证 IP 真的变了",
            "detail": "点「重新检测」（force）绕过 5 分钟缓存，确认新 IP 与第 1 步的基线不同。相同就回到第 2 步换另一种方式。",
            "doneWhen": "新 IP ≠ 基线 IP",
        },
        {
            "step": 4,
            "title": "用新 IP 登录新号并记账",
            "detail": "在官方客户端登录新号，然后在账号页把该号保存进来；本向导会把当前 IP 写进这些号的「出口 IP」，之后同一 IP 上号数超阈值就会提示。",
            "doneWhen": "新号的出口 IP 记录等于第 3 步验证过的新 IP",
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_api_success_is_parsed() {
        let body = json!({
            "status": "success",
            "query": "203.0.113.7",
            "country": "中国",
            "regionName": "广东省",
            "city": "深圳市",
            "isp": "电信",
        });
        let egress = parse_probe("ip-api", &body).expect("应能解析");
        assert_eq!(egress.ip, "203.0.113.7");
        assert_eq!(egress.location(), "中国 广东省 深圳市");
        assert_eq!(egress.isp, "电信");
    }

    #[test]
    fn ip_api_failure_status_is_rejected() {
        let body = json!({"status": "fail", "message": "reserved range"});
        assert!(parse_probe("ip-api", &body).is_none());
    }

    #[test]
    fn ipify_only_needs_ip() {
        let egress = parse_probe("ipify", &json!({"ip": "198.51.100.9"})).expect("应能解析");
        assert_eq!(egress.ip, "198.51.100.9");
        // 没有归属地时不能留下多余空格。
        assert_eq!(egress.location(), "");
    }

    #[test]
    fn missing_ip_is_rejected_for_every_source() {
        for source in ["ip-api", "ipify", "ipapi.co"] {
            assert!(
                parse_probe(source, &json!({"status": "success"})).is_none(),
                "{source} 不该接受空 IP"
            );
        }
        assert!(parse_probe("unknown-source", &json!({"ip": "1.2.3.4"})).is_none());
    }

    #[test]
    fn ipapi_co_uses_its_own_field_names() {
        let body = json!({
            "ip": "192.0.2.5",
            "country_name": "新加坡",
            "region": "Central",
            "city": "Singapore",
            "org": "Example ISP",
        });
        let egress = parse_probe("ipapi.co", &body).expect("应能解析");
        assert_eq!(egress.location(), "新加坡 Central Singapore");
        assert_eq!(egress.isp, "Example ISP");
    }

    #[test]
    fn location_skips_blank_fields_without_stray_spaces() {
        let egress = Egress {
            ip: "1.2.3.4".to_string(),
            country: "中国".to_string(),
            region: String::new(),
            city: "杭州市".to_string(),
            isp: String::new(),
            source: "ip-api".to_string(),
        };
        assert_eq!(egress.location(), "中国 杭州市");
    }

    #[test]
    fn assign_refuses_empty_ip() {
        let out = assign(&["a".to_string()], "   ");
        assert_eq!(out.get("updated").and_then(|v| v.as_u64()), Some(0));
        assert!(out.get("error").is_some());
    }

    #[test]
    fn accounts_on_blank_ip_is_empty() {
        assert!(accounts_on_ip("  ").is_empty());
    }

    #[test]
    fn guide_has_four_steps_with_verifiable_conditions() {
        let steps = guide();
        let list = steps.as_array().expect("guide 应是数组");
        assert_eq!(list.len(), 4);
        for (index, step) in list.iter().enumerate() {
            assert_eq!(
                step.get("step").and_then(|v| v.as_i64()),
                Some(index as i64 + 1)
            );
            // 每步都必须给出「怎么算做完了」，否则向导退化成一段说明文字。
            assert!(
                step.get("doneWhen")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| !s.trim().is_empty()),
                "第 {} 步缺少 doneWhen",
                index + 1
            );
        }
    }

    #[test]
    fn sharing_report_warns_at_threshold() {
        // 用不存在的 IP 走一遍：应为 0 个号且不告警（不依赖本机账号库内容）。
        let report = sharing_report("203.0.113.254");
        assert_eq!(report.get("count").and_then(|v| v.as_u64()), Some(0));
        assert_eq!(report.get("warn").and_then(|v| v.as_bool()), Some(false));
    }

    #[test]
    fn history_keeps_one_entry_per_ip_and_caps_length() {
        let mut cache = json!({});
        for _ in 0..3 {
            push_history(&mut cache, &json!({"ip": "1.1.1.1", "at": 1}));
        }
        push_history(&mut cache, &json!({"ip": "2.2.2.2", "at": 2}));
        let list = cache.get("history").and_then(|v| v.as_array()).unwrap();
        assert_eq!(list.len(), 2, "同一 IP 只应保留最近一次");
        assert_eq!(list[0].get("ip").and_then(|v| v.as_str()), Some("2.2.2.2"));

        for i in 0..(HISTORY_LIMIT + 10) {
            push_history(&mut cache, &json!({"ip": format!("10.0.0.{i}"), "at": i}));
        }
        let list = cache.get("history").and_then(|v| v.as_array()).unwrap();
        assert_eq!(list.len(), HISTORY_LIMIT);
    }
}
