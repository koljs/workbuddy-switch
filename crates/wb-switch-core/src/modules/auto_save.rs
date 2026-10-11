//! 自动保存新号：把本机 WorkBuddy 登录态里出现、但账号库里还没有的账号自动收进来。
//!
//! 解决的问题很具体：账号库里有 5 个号，某天你在官方客户端登录了第 6 个号（也许只是
//! 帮同事看一眼），回头想把 6 个号都管起来，得先想起去点「导入本机账号」。这个模块
//! 把那一步去掉——登录态一变，新号自己就进来了。
//!
//! ## 只增不改
//!
//! 只补账号库里没有的 uid，**已存在的记录一个字段都不动**。理由有两条：
//!
//! 1. 登录态里可能是加密信封 token，而库里存着 OAuth 扫码得到的明文 token。
//!    用信封覆盖明文会让签到 / 积分等依赖明文的功能失效——`account::upsert_collected_account`
//!    里那段「WorkBuddy 5.6 加密态保护」就是在防这件事。
//! 2. 库里的记录可能带着用户自己加的备注、显示字段选择、代理配置。自动流程没有资格
//!    替用户决定这些要不要被官方数据覆盖。
//!
//! 想强制用登录态刷新已有记录，请走「导入本机账号」（`account::import_local`）——
//! 那是一次明确的手动操作，`upsert_collected_account` 的保护规则仍然生效。
//!
//! 顺带一提，「只增不改」也让这个循环天然幂等：跑一次和跑一百次结果一样。

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

use crate::modules::{account, auth_file, config, variant::WbVariant};

/// 扫描间隔。
///
/// 45 秒是个折中：官方客户端登录一次要好几秒，这个粒度足够「登录完回到管理器就看到了」；
/// 而每轮只读两个小 JSON 文件，空转成本可以忽略。
pub const SCAN_INTERVAL: Duration = Duration::from_secs(45);

/// 配置文件名（放在 store 根目录，与 `config.rs` 里其它状态文件一致）。
pub const CONFIG_FILE: &str = "auto_save_config.json";

pub fn config_file() -> PathBuf {
    config::store_dir().join(CONFIG_FILE)
}

/// 从配置文本里解析开关。
///
/// **缺省是开启**：这个功能本身就是「省一步操作」，默认关掉等于没做。文件不存在、
/// JSON 损坏、或没写 `enabled` 字段，一律按开启处理。
pub fn parse_enabled(text: &str) -> bool {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| value.get("enabled").and_then(Value::as_bool))
        .unwrap_or(true)
}

/// 当前是否开启自动保存。
pub fn is_enabled() -> bool {
    match std::fs::read_to_string(config_file()) {
        Ok(text) => parse_enabled(&text),
        Err(_) => true,
    }
}

/// 写开关。返回写入后的配置对象。
pub fn set_enabled(enabled: bool) -> Result<Value, String> {
    let path = config_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败：{e}"))?;
    }
    let value = json!({ "enabled": enabled });
    let text = serde_json::to_string_pretty(&value).map_err(|e| format!("序列化配置失败：{e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("写入自动保存配置失败：{e}"))?;
    Ok(value)
}

/// 当前配置（含缺省值），供 UI 读取。
pub fn load_config() -> Value {
    json!({ "enabled": is_enabled() })
}

/// 该不该把这个采集结果收进账号库。`known` 是同档位已有 uid 的集合。
///
/// 抽成纯函数是为了能直接单测这条策略——它正是「只增不改」的全部内容。
pub fn should_save(collected: &Value, known: &HashSet<String>) -> bool {
    match account::get_str(collected, "uid") {
        Some(uid) => {
            let uid = uid.trim();
            !uid.is_empty() && !known.contains(uid)
        }
        None => false,
    }
}

/// 某个档位账号库里已有的 uid 集合。
///
/// 按档位分开：国内版与国际版是两套账号体系，uid 空间不同，混在一起会把
/// 「国际版刚登录的新号」误判成「国内版已有」，于是永远收不进来。
fn known_uids(variant: WbVariant) -> HashSet<String> {
    account::load_accounts()
        .iter()
        .filter(|existing| account::variant_of(existing) == variant)
        .filter_map(|existing| account::get_str(existing, "uid"))
        .map(|uid| uid.trim().to_string())
        .filter(|uid| !uid.is_empty())
        .collect()
}

/// 扫一遍全部档位，把登录态里出现、账号库里没有的账号补进去。
///
/// 返回 `{ "scanned": <读到登录态的档位数>, "added": [账号 meta, ...] }`。
/// `added` 为空是常态（绝大多数轮次什么都没发生），调用方据此决定要不要提示用户。
pub fn scan_once() -> Value {
    let mut scanned = 0usize;
    let mut added: Vec<Value> = Vec::new();

    for variant in WbVariant::ALL {
        // 该档位没登录 / 登录态读不出来 → 跳过，不是错误。
        let Some(collected) = auth_file::import_from_auth_file(variant) else {
            continue;
        };
        scanned += 1;

        if !should_save(&collected, &known_uids(variant)) {
            continue;
        }
        match account::save_collected_account(collected) {
            Ok(saved) => added.push(account::account_meta(&saved)),
            Err(error) => eprintln!("[自动保存] 写入账号库失败（{}）：{error}", variant.as_str()),
        }
    }

    json!({ "scanned": scanned, "added": added })
}

/// 后台循环体：按间隔扫描，开关关掉时只睡不扫。
///
/// 与 `checkin` / `travel` 的循环同构，由 server 与桌面端各自的
/// `spawn_background_loops` 拉起。
pub async fn run_auto_save_cycle() {
    loop {
        tokio::time::sleep(SCAN_INTERVAL).await;
        if !is_enabled() {
            continue;
        }
        let result = scan_once();
        if let Some(items) = result.get("added").and_then(Value::as_array) {
            for meta in items {
                let name = account::get_str(meta, "nickname")
                    .or_else(|| account::get_str(meta, "email"))
                    .unwrap_or_else(|| "未知".to_string());
                eprintln!("[自动保存] 已收进新账号：{name}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(uids: &[&str]) -> HashSet<String> {
        uids.iter().map(|uid| (*uid).to_string()).collect()
    }

    /// 缺省开启：文件缺失 / 损坏 / 没写字段都当开启，否则这个功能等于默认不存在。
    #[test]
    fn enabled_defaults_to_true_when_missing_or_broken() {
        assert!(parse_enabled(""));
        assert!(parse_enabled("不是 JSON"));
        assert!(parse_enabled("{}"));
        assert!(parse_enabled(r#"{"enabled":null}"#));
        assert!(parse_enabled(r#"{"other":1}"#));
        assert!(parse_enabled(r#"{"enabled":true}"#));
        assert!(!parse_enabled(r#"{"enabled":false}"#));
    }

    /// 库里没有这个 uid → 收。
    #[test]
    fn new_uid_is_saved() {
        let collected = json!({"uid": "u-new", "access_token": "T"});
        assert!(should_save(&collected, &known(&["u-1", "u-2"])));
    }

    /// 库里已经有这个 uid → 一个字段都不动（只增不改）。
    #[test]
    fn existing_uid_is_skipped() {
        let collected = json!({"uid": "u-1", "access_token": "T-newer"});
        assert!(!should_save(&collected, &known(&["u-1"])));
    }

    /// 没有 uid 就没法判断是不是新号——宁可不收，也不要往库里塞重复记录。
    #[test]
    fn missing_or_blank_uid_is_skipped() {
        assert!(!should_save(&json!({"access_token": "T"}), &known(&[])));
        assert!(!should_save(&json!({"uid": ""}), &known(&[])));
        assert!(!should_save(&json!({"uid": "   "}), &known(&[])));
        assert!(!should_save(&json!({"uid": null}), &known(&[])));
    }

    /// uid 前后带空白不该被当成另一个账号。
    #[test]
    fn uid_is_trimmed_before_comparison() {
        let collected = json!({"uid": "  u-1  "});
        assert!(!should_save(&collected, &known(&["u-1"])));
    }

    /// 幂等：收过一次之后再扫，同一个 uid 不再命中。
    #[test]
    fn scanning_is_idempotent() {
        let collected = json!({"uid": "u-9", "access_token": "T"});
        let mut have = known(&["u-1"]);
        assert!(should_save(&collected, &have));
        have.insert("u-9".to_string());
        assert!(!should_save(&collected, &have));
    }

    /// 配置对象形状与 `load_config` 一致，前端才能照着一个字段渲染开关。
    #[test]
    fn config_object_carries_enabled_flag() {
        let cfg = load_config();
        assert!(cfg.get("enabled").and_then(Value::as_bool).is_some());
    }
}
